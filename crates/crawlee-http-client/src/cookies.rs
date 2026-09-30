//! A thread-safe cookie jar, one per session.

use std::collections::HashMap;

use cookie_store::{CookieStore, RawCookie};
use parking_lot::Mutex;
use url::Url;

/// `(domain, path, name)` identifies a cookie in the store.
type CookieKey = (String, String, String);

#[derive(Debug, Default, Clone)]
struct Inner {
    store: CookieStore,
    /// Creation order of every cookie, for the RFC 6265 ordering of the `Cookie` header.
    /// `cookie_store` does not keep it, and iterates cookies in hash order.
    created: HashMap<CookieKey, u64>,
    next_sequence: u64,
}

fn key_of(cookie: &cookie_store::Cookie<'_>) -> CookieKey {
    (
        cookie.domain.as_cow().map(|d| d.into_owned()).unwrap_or_default(),
        String::from(&cookie.path),
        cookie.name().to_owned(),
    )
}

impl Inner {
    /// Assigns a creation sequence number to cookies stored since the last call. A replaced
    /// cookie keeps the creation time of the one it replaced, as RFC 6265 requires.
    fn record_new_cookies(&mut self) {
        for cookie in self.store.iter_any() {
            let key = key_of(cookie);
            if !self.created.contains_key(&key) {
                self.created.insert(key, self.next_sequence);
                self.next_sequence += 1;
            }
        }
    }

    /// Matching cookies ordered like tough-cookie (and RFC 6265 section 5.4): longer paths first,
    /// then earlier creation.
    fn ordered_matches(&self, url: &Url) -> Vec<(&str, &str)> {
        let mut matches: Vec<(usize, u64, &cookie_store::Cookie<'static>)> = self
            .store
            .matches(url)
            .into_iter()
            .map(|cookie| {
                let sequence = self.created.get(&key_of(cookie)).copied().unwrap_or(u64::MAX);
                (String::from(&cookie.path).len(), sequence, cookie)
            })
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        matches.into_iter().map(|(_, _, cookie)| cookie.name_value()).collect()
    }
}

/// Cookies of one session. Shared between concurrent requests of that session.
#[derive(Debug, Default)]
pub struct CookieJar {
    inner: Mutex<Inner>,
}

impl Clone for CookieJar {
    fn clone(&self) -> Self {
        CookieJar { inner: Mutex::new(self.inner.lock().clone()) }
    }
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `Cookie` header value for `url`, or `None` when no cookie applies.
    pub fn cookie_header(&self, url: &Url) -> Option<String> {
        let inner = self.inner.lock();
        let mut header = String::new();
        for (name, value) in inner.ordered_matches(url) {
            if !header.is_empty() {
                header.push_str("; ");
            }
            header.push_str(name);
            header.push('=');
            header.push_str(value);
        }
        (!header.is_empty()).then_some(header)
    }

    /// Stores a cookie given in `Set-Cookie` syntax, as if `url` had set it.
    pub fn set_cookie(&self, cookie: &str, url: &Url) -> bool {
        let mut inner = self.inner.lock();
        let stored = inner.store.parse(cookie, url).is_ok();
        inner.record_new_cookies();
        stored
    }

    /// Stores every `Set-Cookie` header of a response received from `url`.
    pub fn store_response_cookies<'a>(&self, set_cookie_headers: impl Iterator<Item = &'a str>, url: &Url) {
        let cookies: Vec<RawCookie<'static>> =
            set_cookie_headers.filter_map(|header| RawCookie::parse(header.to_owned()).ok()).collect();
        if cookies.is_empty() {
            return;
        }
        let mut inner = self.inner.lock();
        // One at a time, so cookies of one response get creation times in header order.
        for cookie in cookies {
            inner.store.store_response_cookies(std::iter::once(cookie), url);
            inner.record_new_cookies();
        }
    }

    /// `(name, value)` pairs that would be sent to `url`, in `Cookie` header order.
    pub fn cookies_for(&self, url: &Url) -> Vec<(String, String)> {
        self.inner
            .lock()
            .ordered_matches(url)
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.store.clear();
        inner.created.clear();
    }

    /// Merges the jar with cookies from an explicit `Cookie` header (which win on name clashes)
    /// without persisting the header's cookies into the jar, as Crawlee for JS does.
    pub fn merged_cookie_header(&self, url: &Url, request_cookie_header: &str) -> Option<String> {
        let merged = self.clone();
        for pair in request_cookie_header.split(';').map(str::trim).filter(|pair| !pair.is_empty()) {
            merged.set_cookie(pair, url);
        }
        merged.cookie_header(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_sends_cookies() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/a").unwrap();
        jar.store_response_cookies(["sid=1; Path=/", "theme=dark"].into_iter(), &url);
        assert_eq!(jar.cookie_header(&url).as_deref(), Some("sid=1; theme=dark"));
        assert_eq!(jar.cookie_header(&Url::parse("https://other.com/").unwrap()), None);

        let merged = jar.merged_cookie_header(&url, "sid=override; extra=x").unwrap();
        assert!(merged.contains("sid=override") && merged.contains("extra=x") && merged.contains("theme=dark"));
        // The header's cookies were not persisted.
        assert_eq!(jar.cookie_header(&url).as_deref(), Some("sid=1; theme=dark"));
    }

    #[test]
    fn header_order_follows_rfc6265() {
        let jar = CookieJar::new();
        let url = Url::parse("https://example.com/shop/cart").unwrap();
        for header in ["z=1; Path=/", "a=2; Path=/", "deep=3; Path=/shop", "m=4; Path=/"] {
            jar.store_response_cookies(std::iter::once(header), &url);
        }
        // Longer paths first, then creation order (not alphabetical, not hash order).
        assert_eq!(jar.cookie_header(&url).as_deref(), Some("deep=3; z=1; a=2; m=4"));

        // Replacing a cookie keeps its original creation time.
        jar.store_response_cookies(std::iter::once("z=updated; Path=/"), &url);
        assert_eq!(jar.cookie_header(&url).as_deref(), Some("deep=3; z=updated; a=2; m=4"));
    }
}
