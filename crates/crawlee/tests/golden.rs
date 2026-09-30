//! Replays the golden files generated from Crawlee for JS (`conformance/oracle`).
//!
//! Every case must match, except the ones listed in [`ALLOWED_DIFFERENCES`] (documented in
//! `conformance/allowed-differences.md`). A listed case that starts matching fails the test too,
//! so the list never goes stale.

use std::collections::BTreeSet;

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crawlee::core::request::unique_key_to_request_id;
use crawlee::utils::links::{extract_charset_from_html_bytes, extract_links};
use crawlee::utils::url::{matches_enqueue_strategy, normalize_url, registrable_domain};
use crawlee::{EnqueueStrategy, Request};

/// `(case id, reason)`; see `conformance/allowed-differences.md`.
const ALLOWED_DIFFERENCES: &[(&str, &str)] = &[];

fn golden<T: for<'de> Deserialize<'de>>(name: &str) -> Vec<T> {
    let path = format!("{}/../../conformance/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("cannot read {path}: {err}"));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("cannot parse {path}: {err}"))
}

/// Collects mismatches and checks them against the allow-list at the end.
struct Report {
    suite: &'static str,
    mismatches: Vec<(String, String)>,
}

impl Report {
    fn new(suite: &'static str) -> Self {
        Report { suite, mismatches: Vec::new() }
    }

    fn check<T: PartialEq + std::fmt::Debug>(
        &mut self,
        id: impl std::fmt::Display,
        actual: T,
        expected: T,
        input: &str,
    ) {
        if actual != expected {
            self.mismatches.push((
                format!("{}/{id}", self.suite),
                format!("input: {input}\n    rust: {actual:?}\n      js: {expected:?}"),
            ));
        }
    }

    fn finish(self) {
        let allowed: BTreeSet<&str> = ALLOWED_DIFFERENCES
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| id.starts_with(&format!("{}/", self.suite)))
            .collect();
        let found: BTreeSet<&str> = self.mismatches.iter().map(|(id, _)| id.as_str()).collect();

        let unexpected: Vec<String> = self
            .mismatches
            .iter()
            .filter(|(id, _)| !allowed.contains(id.as_str()))
            .map(|(id, detail)| format!("{id}\n    {detail}"))
            .collect();
        let stale: Vec<&&str> = allowed.iter().filter(|id| !found.contains(**id)).collect();

        assert!(
            unexpected.is_empty(),
            "{} case(s) differ from Crawlee for JS:\n{}",
            unexpected.len(),
            unexpected.join("\n")
        );
        assert!(stale.is_empty(), "allowed differences that no longer differ (remove them): {stale:?}");
    }
}

#[test]
fn normalize_url_matches_js() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        input: String,
        keep_fragment: bool,
        expected: Option<String>,
    }
    let mut report = Report::new("normalize_url");
    for (i, case) in golden::<Case>("normalize_url").into_iter().enumerate() {
        report.check(i, normalize_url(&case.input, case.keep_fragment), case.expected, &case.input);
    }
    report.finish();
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RequestOptions {
    url: String,
    method: Option<String>,
    payload: Option<String>,
    unique_key: Option<String>,
    use_extended_unique_key: bool,
    keep_url_fragment: bool,
    label: Option<String>,
    user_data: Option<serde_json::Map<String, Value>>,
    headers: Option<indexmap_like::Headers>,
    crawl_depth: Option<u32>,
    max_retries: Option<u32>,
    skip_navigation: Option<bool>,
    session_id: Option<String>,
    no_retry: bool,
    enqueue_strategy: Option<EnqueueStrategy>,
}

mod indexmap_like {
    pub type Headers = serde_json::Map<String, serde_json::Value>;
}

fn build_request(options: RequestOptions) -> Request {
    let mut builder = Request::builder(options.url)
        .use_extended_unique_key(options.use_extended_unique_key)
        .keep_url_fragment(options.keep_url_fragment)
        .no_retry(options.no_retry);
    if let Some(method) = options.method {
        builder = builder.method(method);
    }
    if let Some(payload) = options.payload {
        builder = builder.payload(payload);
    }
    if let Some(key) = options.unique_key {
        builder = builder.unique_key(key);
    }
    if let Some(user_data) = options.user_data {
        builder = builder.user_data_map(user_data);
    }
    if let Some(label) = options.label {
        builder = builder.label(label);
    }
    if let Some(headers) = options.headers {
        builder = builder.headers(headers.into_iter().map(|(k, v)| (k, v.as_str().unwrap_or_default().to_owned())));
    }
    if let Some(depth) = options.crawl_depth {
        builder = builder.crawl_depth(depth);
    }
    if let Some(retries) = options.max_retries {
        builder = builder.max_retries(retries);
    }
    if let Some(skip) = options.skip_navigation {
        builder = builder.skip_navigation(skip);
    }
    if let Some(session) = options.session_id {
        builder = builder.session_id(session);
    }
    if let Some(strategy) = options.enqueue_strategy {
        builder = builder.enqueue_strategy(strategy);
    }
    builder.build().expect("valid request options")
}

#[test]
fn unique_keys_and_request_ids_match_js() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        options: Value,
        unique_key: String,
        request_id: String,
    }
    let mut report = Report::new("unique_key");
    for (i, case) in golden::<Case>("unique_key").into_iter().enumerate() {
        let request = build_request(serde_json::from_value(case.options.clone()).unwrap());
        let input = case.options.to_string();
        report.check(format!("{i}/key"), request.unique_key.clone(), case.unique_key, &input);
        report.check(format!("{i}/id"), unique_key_to_request_id(&request.unique_key), case.request_id, &input);
    }
    report.finish();
}

#[test]
fn request_json_matches_js() {
    #[derive(Deserialize)]
    struct Case {
        options: Value,
        json: Value,
    }
    let mut report = Report::new("request_json");
    for (i, case) in golden::<Case>("request_json").into_iter().enumerate() {
        let input = case.options.to_string();
        let built = build_request(serde_json::from_value(case.options.clone()).unwrap());
        report.check(format!("{i}/serialize"), serde_json::to_value(&built).unwrap(), case.json.clone(), &input);

        let parsed: Request = serde_json::from_value(case.json.clone()).unwrap();
        report.check(format!("{i}/round-trip"), serde_json::to_value(&parsed).unwrap(), case.json, &input);
    }
    report.finish();
}

#[test]
fn registrable_domains_match_tldts() {
    #[derive(Deserialize)]
    struct Case {
        hostname: String,
        expected: Option<String>,
    }
    let mut report = Report::new("registrable_domain");
    for case in golden::<Case>("registrable_domain") {
        // `URL.hostname` of an IDN is its punycode form; tldts gets hostnames that way.
        report.check(&case.hostname, registrable_domain(&case.hostname), case.expected, &case.hostname);
    }
    report.finish();
}

#[test]
fn enqueue_strategies_match_js() {
    #[derive(Deserialize)]
    struct Case {
        strategy: EnqueueStrategy,
        origin: String,
        target: String,
        expected: bool,
    }
    let mut report = Report::new("enqueue_strategy");
    for (i, case) in golden::<Case>("enqueue_strategy").into_iter().enumerate() {
        let actual = matches_enqueue_strategy(
            case.strategy,
            &Url::parse(&case.target).unwrap(),
            &Url::parse(&case.origin).unwrap(),
        );
        let input = format!("{} {} -> {}", case.strategy, case.origin, case.target);
        report.check(i, actual, case.expected, &input);
    }
    report.finish();
}

#[test]
fn streaming_link_extraction_matches_cheerio() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        id: String,
        html: String,
        selector: String,
        base_url: String,
        expected: Vec<String>,
    }
    let mut report = Report::new("extract_links");
    for case in golden::<Case>("extract_links") {
        let links: Vec<String> =
            extract_links(case.html.as_bytes(), &case.selector, &Url::parse(&case.base_url).unwrap())
                .unwrap()
                .into_iter()
                .map(String::from)
                .collect();
        report.check(&case.id, links, case.expected, &case.html);
    }
    report.finish();
}

#[test]
fn charset_prescan_matches_js() {
    #[derive(Deserialize)]
    struct Case {
        html: String,
        expected: Option<String>,
    }
    let mut report = Report::new("charset_prescan");
    for (i, case) in golden::<Case>("charset_prescan").into_iter().enumerate() {
        // The JS side encodes the HTML as latin1; every char here is ASCII.
        report.check(i, extract_charset_from_html_bytes(case.html.as_bytes()), case.expected, &case.html);
    }
    report.finish();
}

#[test]
fn key_value_store_json_formatting_matches_js() {
    #[derive(Deserialize)]
    struct Case {
        value: Value,
        text: String,
    }
    let mut report = Report::new("kvs_json");
    for (i, case) in golden::<Case>("kvs_json").into_iter().enumerate() {
        let bytes = crawlee::core::storage::key_value_store::serialize_json(&case.value).unwrap();
        report.check(i, String::from_utf8(bytes.to_vec()).unwrap(), case.text, &case.value.to_string());
    }
    report.finish();
}
