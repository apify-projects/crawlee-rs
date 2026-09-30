//! [`Request`]: a URL to crawl, plus the processing state a request queue keeps about it.
//!
//! The JSON form of a request is shared with Crawlee for JavaScript and Python (it is what request
//! queues store), so field names and the `userData.__crawlee` bag follow the JS `RequestSchema`.

use base64::Engine as _;
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crawlee_utils::EnqueueStrategy;
use crawlee_utils::url::normalize_url;

/// Length of [`Request::id`], as on the Apify platform.
pub const REQUEST_ID_LENGTH: usize = 15;

/// Crawlee's own per-request settings, stored under `userData.__crawlee`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawleeRequestData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_navigation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crawl_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enqueue_strategy: Option<EnqueueStrategy>,
    /// `RequestState` of Crawlee for JS, kept as its numeric value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<u8>,
    /// Keys written by other implementations, preserved on round trips.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CrawleeRequestData {
    fn is_empty(&self) -> bool {
        self.skip_navigation.is_none()
            && self.crawl_depth.is_none()
            && self.session_id.is_none()
            && self.max_retries.is_none()
            && self.enqueue_strategy.is_none()
            && self.state.is_none()
            && self.extra.is_empty()
    }
}

/// A URL to crawl, optionally with a method, headers, payload and user data.
///
/// ```
/// use crawlee_core::Request;
///
/// let request = Request::new("HTTP://www.EXAMPLE.com/something/");
/// assert_eq!(request.unique_key, "http://www.example.com/something");
///
/// let request = Request::builder("https://example.com/product/1").label("DETAIL").build().unwrap();
/// assert_eq!(request.label(), Some("DETAIL"));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// Id assigned by the request queue (derived from `unique_key`).
    pub id: Option<String>,
    pub url: String,
    /// Two requests with the same unique key are considered the same page and deduplicated.
    pub unique_key: String,
    /// Upper-cased HTTP method.
    pub method: String,
    pub payload: Option<String>,
    pub headers: IndexMap<String, String>,
    /// Arbitrary JSON data attached to the request. The reserved `__crawlee` key is kept separately
    /// in [`Request::crawlee`] and merged back on serialization.
    pub user_data: Map<String, Value>,
    pub crawlee: CrawleeRequestData,
    /// When set, the request is never retried.
    pub no_retry: bool,
    pub retry_count: u32,
    pub error_messages: Vec<String>,
    /// The URL that was actually loaded, after redirects.
    pub loaded_url: Option<String>,
    /// ISO 8601 time the request was handled; `None` while pending.
    pub handled_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("Request with GET method cannot have a payload.")]
    GetWithPayload,
    #[error("`always_enqueue` cannot be used together with a custom `unique_key`.")]
    AlwaysEnqueueWithUniqueKey,
    #[error("Request URL must be a non-empty string.")]
    EmptyUrl,
}

impl Request {
    /// A `GET` request for `url`, with the unique key computed from the normalized URL.
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        let unique_key = compute_unique_key(&url, "GET", None, false, false, false);
        Request {
            id: None,
            url,
            unique_key,
            method: "GET".to_owned(),
            payload: None,
            headers: IndexMap::new(),
            user_data: Map::new(),
            crawlee: CrawleeRequestData::default(),
            no_retry: false,
            retry_count: 0,
            error_messages: Vec::new(),
            loaded_url: None,
            handled_at: None,
        }
    }

    pub fn builder(url: impl Into<String>) -> RequestBuilder {
        RequestBuilder::new(url)
    }

    /// `userData.label`, used by the router.
    pub fn label(&self) -> Option<&str> {
        self.user_data.get("label").and_then(Value::as_str)
    }

    pub fn set_label(&mut self, label: impl Into<String>) {
        self.user_data.insert("label".to_owned(), Value::String(label.into()));
    }

    /// Deserializes `user_data` into a typed struct.
    pub fn user_data_as<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        T::deserialize(Value::Object(self.user_data.clone()))
    }

    pub fn crawl_depth(&self) -> u32 {
        self.crawlee.crawl_depth.unwrap_or(0)
    }

    pub fn skip_navigation(&self) -> bool {
        self.crawlee.skip_navigation.unwrap_or(false)
    }

    pub fn max_retries(&self) -> Option<u32> {
        self.crawlee.max_retries
    }

    pub fn session_id(&self) -> Option<&str> {
        self.crawlee.session_id.as_deref()
    }

    pub fn is_handled(&self) -> bool {
        self.handled_at.is_some()
    }

    /// Records an error message, as `pushErrorMessage` does in Crawlee for JS.
    pub fn push_error_message(&mut self, message: impl Into<String>) {
        self.error_messages.push(message.into());
    }

    /// The queue id derived from the unique key, as on the Apify platform.
    pub fn compute_id(&self) -> String {
        unique_key_to_request_id(&self.unique_key)
    }
}

impl From<&str> for Request {
    fn from(url: &str) -> Self {
        Request::new(url)
    }
}

impl From<String> for Request {
    fn from(url: String) -> Self {
        Request::new(url)
    }
}

impl From<url::Url> for Request {
    fn from(url: url::Url) -> Self {
        Request::new(String::from(url))
    }
}

impl From<&url::Url> for Request {
    fn from(url: &url::Url) -> Self {
        Request::new(url.as_str())
    }
}

/// Builder for [`Request`], mirroring `RequestOptions` of Crawlee for JS.
#[derive(Clone, Debug, Default)]
#[must_use]
pub struct RequestBuilder {
    url: String,
    method: Option<String>,
    payload: Option<String>,
    headers: IndexMap<String, String>,
    user_data: Map<String, Value>,
    label: Option<String>,
    unique_key: Option<String>,
    keep_url_fragment: bool,
    use_extended_unique_key: bool,
    always_enqueue: bool,
    no_retry: bool,
    crawlee: CrawleeRequestData,
}

impl RequestBuilder {
    pub fn new(url: impl Into<String>) -> Self {
        RequestBuilder { url: url.into(), ..Default::default() }
    }

    pub fn method(mut self, method: impl AsRef<str>) -> Self {
        self.method = Some(method.as_ref().to_ascii_uppercase());
        self
    }

    pub fn payload(mut self, payload: impl Into<String>) -> Self {
        self.payload = Some(payload.into());
        self
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn headers(mut self, headers: impl IntoIterator<Item = (String, String)>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// Replaces the user data. Fails if `data` does not serialize to a JSON object.
    pub fn user_data<T: Serialize>(mut self, data: &T) -> Result<Self, serde_json::Error> {
        match serde_json::to_value(data)? {
            Value::Object(map) => {
                self.user_data = map;
                Ok(self)
            }
            other => Err(serde::ser::Error::custom(format!("user data must be a JSON object, got {other}"))),
        }
    }

    pub fn user_data_map(mut self, data: Map<String, Value>) -> Self {
        self.user_data = data;
        self
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn unique_key(mut self, unique_key: impl Into<String>) -> Self {
        self.unique_key = Some(unique_key.into());
        self
    }

    pub fn keep_url_fragment(mut self, keep: bool) -> Self {
        self.keep_url_fragment = keep;
        self
    }

    /// Includes the method and a payload hash in the unique key.
    pub fn use_extended_unique_key(mut self, extended: bool) -> Self {
        self.use_extended_unique_key = extended;
        self
    }

    /// Makes the unique key random, so the request is enqueued even if the URL was seen before.
    pub fn always_enqueue(mut self, always: bool) -> Self {
        self.always_enqueue = always;
        self
    }

    pub fn no_retry(mut self, no_retry: bool) -> Self {
        self.no_retry = no_retry;
        self
    }

    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.crawlee.max_retries = Some(max_retries);
        self
    }

    pub fn skip_navigation(mut self, skip: bool) -> Self {
        self.crawlee.skip_navigation = Some(skip);
        self
    }

    pub fn crawl_depth(mut self, depth: u32) -> Self {
        self.crawlee.crawl_depth = Some(depth);
        self
    }

    pub fn session_id(mut self, session_id: impl Into<String>) -> Self {
        self.crawlee.session_id = Some(session_id.into());
        self
    }

    pub fn enqueue_strategy(mut self, strategy: EnqueueStrategy) -> Self {
        self.crawlee.enqueue_strategy = Some(strategy);
        self
    }

    pub fn build(self) -> Result<Request, RequestError> {
        if self.url.is_empty() {
            return Err(RequestError::EmptyUrl);
        }
        let method = self.method.unwrap_or_else(|| "GET".to_owned());
        if method == "GET" && self.payload.as_deref().is_some_and(|p| !p.is_empty()) {
            return Err(RequestError::GetWithPayload);
        }
        if self.unique_key.is_some() && self.always_enqueue {
            return Err(RequestError::AlwaysEnqueueWithUniqueKey);
        }

        let unique_key = match self.unique_key.filter(|key| !key.is_empty()) {
            Some(key) => key,
            None => compute_unique_key(
                &self.url,
                &method,
                self.payload.as_deref(),
                self.keep_url_fragment,
                self.use_extended_unique_key,
                self.always_enqueue,
            ),
        };

        let mut user_data = self.user_data;
        let explicit = self.crawlee;
        let crawlee = match user_data.remove("__crawlee") {
            // `__crawlee` passed in user data (e.g. copied from another request) is honored.
            Some(bag @ Value::Object(_)) => {
                let mut merged: CrawleeRequestData = serde_json::from_value(bag).unwrap_or_default();
                merged.skip_navigation = explicit.skip_navigation.or(merged.skip_navigation);
                merged.max_retries = explicit.max_retries.or(merged.max_retries);
                merged.session_id = explicit.session_id.or(merged.session_id);
                // A stored request keeps the depth and strategy it was enqueued under.
                merged.crawl_depth = merged.crawl_depth.or(explicit.crawl_depth);
                merged.enqueue_strategy = merged.enqueue_strategy.or(explicit.enqueue_strategy);
                merged
            }
            _ => explicit,
        };

        if let Some(label) = self.label {
            user_data.insert("label".to_owned(), Value::String(label));
        }

        Ok(Request {
            id: None,
            url: self.url,
            unique_key,
            method,
            payload: self.payload,
            headers: self.headers,
            user_data,
            crawlee,
            no_retry: self.no_retry,
            retry_count: 0,
            error_messages: Vec::new(),
            loaded_url: None,
            handled_at: None,
        })
    }
}

/// Computes a unique key like `Request.computeUniqueKey` in Crawlee for JS:
/// the normalized URL (or the URL as-is when it cannot be normalized), optionally prefixed with
/// `METHOD|payloadHash|` and, for `always_enqueue`, a random id.
pub fn compute_unique_key(
    url: &str,
    method: &str,
    payload: Option<&str>,
    keep_url_fragment: bool,
    use_extended_unique_key: bool,
    always_enqueue: bool,
) -> String {
    let normalized_url = normalize_url(url, keep_url_fragment).unwrap_or_else(|| url.to_owned());

    let unique_key = if use_extended_unique_key {
        let method = method.to_ascii_uppercase();
        let payload_hash = payload.filter(|p| !p.is_empty()).map(hash_payload).unwrap_or_default();
        format!("{method}|{payload_hash}|{normalized_url}")
    } else {
        normalized_url
    };

    if always_enqueue { format!("{}|{unique_key}", crypto_random_object_id(17)) } else { unique_key }
}

/// First 8 characters of the base64 SHA-256 of the payload, with `+`, `/` and `=` removed.
pub fn hash_payload(payload: &str) -> String {
    let mut out = sha256_base64_stripped(payload.as_bytes());
    out.truncate(8);
    out
}

/// The request id the Apify platform (and every Crawlee storage) derives from a unique key.
pub fn unique_key_to_request_id(unique_key: &str) -> String {
    let mut out = sha256_base64_stripped(unique_key.as_bytes());
    out.truncate(REQUEST_ID_LENGTH);
    out
}

fn sha256_base64_stripped(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut encoded = base64::engine::general_purpose::STANDARD.encode(digest);
    encoded.retain(|c| !matches!(c, '+' | '/' | '='));
    encoded
}

/// A random alphanumeric id, like `cryptoRandomObjectId` from `@apify/utilities`.
pub fn crypto_random_object_id(length: usize) -> String {
    use rand::Rng as _;
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    (0..length).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

// ---------------------------------------------------------------------------------------------
// Serialization: the JSON wire format shared with Crawlee for JS (`RequestSchema`).
// ---------------------------------------------------------------------------------------------

struct UserDataWithBag<'a> {
    user_data: &'a Map<String, Value>,
    crawlee: &'a CrawleeRequestData,
}

impl Serialize for UserDataWithBag<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let include_bag = !self.crawlee.is_empty();
        let mut map = serializer.serialize_map(Some(self.user_data.len() + usize::from(include_bag)))?;
        for (key, value) in self.user_data {
            map.serialize_entry(key, value)?;
        }
        if include_bag {
            map.serialize_entry("__crawlee", self.crawlee)?;
        }
        map.end()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestWireRef<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: &'a Option<String>,
    url: &'a str,
    unique_key: &'a str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: &'a Option<String>,
    no_retry: bool,
    retry_count: u32,
    error_messages: &'a [String],
    headers: &'a IndexMap<String, String>,
    user_data: UserDataWithBag<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loaded_url: &'a Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    handled_at: &'a Option<String>,
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        RequestWireRef {
            id: &self.id,
            url: &self.url,
            unique_key: &self.unique_key,
            method: &self.method,
            payload: &self.payload,
            no_retry: self.no_retry,
            retry_count: self.retry_count,
            error_messages: &self.error_messages,
            headers: &self.headers,
            user_data: UserDataWithBag { user_data: &self.user_data, crawlee: &self.crawlee },
            loaded_url: &self.loaded_url,
            handled_at: &self.handled_at,
        }
        .serialize(serializer)
    }
}

fn default_method() -> String {
    "GET".to_owned()
}

/// `null`s are accepted for every optional field; other implementations write them.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestWire {
    #[serde(default)]
    id: Option<String>,
    url: String,
    unique_key: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default)]
    payload: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    no_retry: bool,
    #[serde(default, deserialize_with = "null_as_default")]
    retry_count: u32,
    #[serde(default, deserialize_with = "null_as_default")]
    error_messages: Vec<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    headers: IndexMap<String, String>,
    #[serde(default, deserialize_with = "null_as_default")]
    user_data: Map<String, Value>,
    #[serde(default)]
    loaded_url: Option<String>,
    #[serde(default)]
    handled_at: Option<String>,
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut wire = RequestWire::deserialize(deserializer)?;
        let crawlee = match wire.user_data.remove("__crawlee") {
            Some(bag @ Value::Object(_)) => serde_json::from_value(bag).map_err(serde::de::Error::custom)?,
            _ => CrawleeRequestData::default(),
        };
        Ok(Request {
            id: wire.id,
            url: wire.url,
            unique_key: wire.unique_key,
            method: wire.method.to_ascii_uppercase(),
            payload: wire.payload,
            headers: wire.headers,
            user_data: wire.user_data,
            crawlee,
            no_retry: wire.no_retry,
            retry_count: wire.retry_count,
            error_messages: wire.error_messages,
            loaded_url: wire.loaded_url,
            handled_at: wire.handled_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unique_keys() {
        assert_eq!(Request::new("https://example.com/a/").unique_key, "https://example.com/a");

        let extended = Request::builder("https://example.com/a")
            .method("post")
            .payload("{\"q\":1}")
            .use_extended_unique_key(true)
            .build()
            .unwrap();
        assert_eq!(extended.method, "POST");
        assert_eq!(extended.unique_key, format!("POST|{}|https://example.com/a", hash_payload("{\"q\":1}")));

        let random = Request::builder("https://example.com/a").always_enqueue(true).build().unwrap();
        assert!(random.unique_key.ends_with("|https://example.com/a"));
        assert_eq!(random.unique_key.len(), 17 + 1 + "https://example.com/a".len());

        assert_eq!(Request::builder("https://example.com").payload("x").build(), Err(RequestError::GetWithPayload));
    }

    #[test]
    fn request_id_matches_platform() {
        // Value produced by `uniqueKeyToRequestId` in Crawlee for JS.
        let id = unique_key_to_request_id("https://example.com");
        assert_eq!(id.len(), REQUEST_ID_LENGTH);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn json_round_trip_keeps_crawlee_bag() {
        let request = Request::builder("https://example.com/x")
            .label("DETAIL")
            .crawl_depth(2)
            .enqueue_strategy(EnqueueStrategy::SameDomain)
            .build()
            .unwrap();

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(
            json,
            json!({
                "url": "https://example.com/x",
                "uniqueKey": "https://example.com/x",
                "method": "GET",
                "noRetry": false,
                "retryCount": 0,
                "errorMessages": [],
                "headers": {},
                "userData": { "label": "DETAIL", "__crawlee": { "crawlDepth": 2, "enqueueStrategy": "same-domain" } },
            })
        );

        let back: Request = serde_json::from_value(json).unwrap();
        assert_eq!(back, request);
    }

    #[test]
    fn deserializes_js_written_requests() {
        let back: Request = serde_json::from_value(json!({
            "id": "abc",
            "url": "https://example.com",
            "uniqueKey": "https://example.com",
            "method": "GET",
            "payload": null,
            "headers": null,
            "userData": { "__crawlee": { "state": 4, "futureField": true } },
            "handledAt": "2026-01-01T00:00:00.000Z"
        }))
        .unwrap();
        assert_eq!(back.crawlee.state, Some(4));
        assert_eq!(back.crawlee.extra.get("futureField"), Some(&json!(true)));
        assert!(back.is_handled());
        assert!(back.user_data.is_empty());
    }

    #[test]
    fn typed_user_data() {
        #[derive(Deserialize, PartialEq, Debug)]
        struct Data {
            label: String,
            page: u32,
        }
        let request = Request::builder("https://example.com")
            .user_data(&json!({ "page": 3 }))
            .unwrap()
            .label("LIST")
            .build()
            .unwrap();
        assert_eq!(request.user_data_as::<Data>().unwrap(), Data { label: "LIST".into(), page: 3 });
    }
}
