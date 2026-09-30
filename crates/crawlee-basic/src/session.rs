//! Sessions and the session pool.
//!
//! A session is the unit of identity the crawler rotates when it gets blocked: it carries its
//! proxy and cookies and accumulates an error score.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::Rng as _;

use crawlee_http_client::CookieJar;

use crate::proxy::{ProxyInfo, ProxySource};

/// Session limits, with the defaults of Crawlee for JS.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Age after which the session is retired.
    pub max_age: Duration,
    /// The session is retired when its error score reaches this value.
    pub max_error_score: f64,
    /// How much a success lowers the error score.
    pub error_score_decrement: f64,
    /// Uses after which the session is retired.
    pub max_usage_count: u32,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            max_age: Duration::from_secs(3000),
            max_error_score: 3.0,
            error_score_decrement: 0.5,
            max_usage_count: 50,
        }
    }
}

#[derive(Debug)]
struct SessionState {
    error_score: f64,
    usage_count: u32,
    retired: bool,
}

/// One identity: an id, a proxy, a cookie jar and a health score.
#[derive(Debug)]
pub struct Session {
    id: String,
    cookie_jar: Arc<CookieJar>,
    proxy_info: Option<ProxyInfo>,
    created_at: Instant,
    options: SessionOptions,
    state: Mutex<SessionState>,
}

impl Session {
    pub fn new(id: String, proxy_info: Option<ProxyInfo>, options: SessionOptions) -> Self {
        Session {
            id,
            cookie_jar: Arc::new(CookieJar::new()),
            proxy_info,
            created_at: Instant::now(),
            options,
            state: Mutex::new(SessionState { error_score: 0.0, usage_count: 0, retired: false }),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn cookie_jar(&self) -> &Arc<CookieJar> {
        &self.cookie_jar
    }

    pub fn proxy_info(&self) -> Option<&ProxyInfo> {
        self.proxy_info.as_ref()
    }

    pub fn error_score(&self) -> f64 {
        self.state.lock().error_score
    }

    pub fn usage_count(&self) -> u32 {
        self.state.lock().usage_count
    }

    pub fn is_retired(&self) -> bool {
        self.state.lock().retired
    }

    pub fn is_expired(&self) -> bool {
        self.created_at.elapsed() >= self.options.max_age
    }

    pub fn is_blocked(&self) -> bool {
        self.state.lock().error_score >= self.options.max_error_score
    }

    pub fn is_max_usage_reached(&self) -> bool {
        self.state.lock().usage_count >= self.options.max_usage_count
    }

    /// Whether the pool may hand this session out.
    pub fn is_usable(&self) -> bool {
        let state = self.state.lock();
        !state.retired
            && state.error_score < self.options.max_error_score
            && state.usage_count < self.options.max_usage_count
            && self.created_at.elapsed() < self.options.max_age
    }

    /// The request succeeded: count the use and lower the error score.
    pub fn mark_good(&self) {
        let mut state = self.state.lock();
        state.usage_count += 1;
        state.error_score = (state.error_score - self.options.error_score_decrement).max(0.0);
    }

    /// The request failed: count the use and raise the error score.
    pub fn mark_bad(&self) {
        let mut state = self.state.lock();
        state.usage_count += 1;
        state.error_score += 1.0;
    }

    /// Takes the session out of rotation for good.
    pub fn retire(&self) {
        let mut state = self.state.lock();
        state.retired = true;
        state.usage_count += 1;
    }
}

/// Pool options, with the defaults of Crawlee for JS.
#[derive(Clone, Debug)]
pub struct SessionPoolOptions {
    pub max_pool_size: usize,
    pub session_options: SessionOptions,
}

impl Default for SessionPoolOptions {
    fn default() -> Self {
        SessionPoolOptions { max_pool_size: 1000, session_options: SessionOptions::default() }
    }
}

/// Hands out sessions: new ones until the pool is full, then random usable ones, replacing
/// sessions that are no longer usable (the `random` strategy of Crawlee for JS).
pub struct SessionPool {
    options: SessionPoolOptions,
    proxies: Option<Arc<dyn ProxySource>>,
    sessions: Mutex<Vec<Arc<Session>>>,
}

impl std::fmt::Debug for SessionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPool").field("options", &self.options).finish_non_exhaustive()
    }
}

impl SessionPool {
    pub fn new(options: SessionPoolOptions, proxies: Option<Arc<dyn ProxySource>>) -> Self {
        SessionPool { options, proxies, sessions: Mutex::new(Vec::new()) }
    }

    fn create_session(&self) -> Arc<Session> {
        let id = format!("session_{}", crawlee_core::request::crypto_random_object_id(10));
        let proxy = self.proxies.as_ref().and_then(|proxies| proxies.new_proxy_info(&id));
        Arc::new(Session::new(id, proxy, self.options.session_options.clone()))
    }

    /// A session for the next request.
    pub fn get_session(&self) -> Arc<Session> {
        let mut sessions = self.sessions.lock();
        if sessions.len() < self.options.max_pool_size.max(1) {
            let session = self.create_session();
            sessions.push(session.clone());
            return session;
        }

        let index = rand::rng().random_range(0..sessions.len());
        if sessions[index].is_usable() {
            return sessions[index].clone();
        }

        sessions.retain(|session| session.is_usable());
        let session = self.create_session();
        sessions.push(session.clone());
        session
    }

    /// The usable session with this id, if any.
    pub fn get_session_by_id(&self, id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().iter().find(|session| session.id() == id && session.is_usable()).cloned()
    }

    pub fn usable_count(&self) -> usize {
        self.sessions.lock().iter().filter(|session| session.is_usable()).count()
    }

    pub fn retired_count(&self) -> usize {
        self.sessions.lock().iter().filter(|session| !session.is_usable()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring_and_retirement() {
        let session = Session::new("s".into(), None, SessionOptions::default());
        session.mark_bad();
        session.mark_bad();
        assert!(session.is_usable());
        session.mark_good();
        assert_eq!(session.error_score(), 1.5);
        session.mark_bad();
        session.mark_bad();
        assert!(session.is_blocked());
        assert!(!session.is_usable());

        let other = Session::new("o".into(), None, SessionOptions::default());
        other.retire();
        assert!(!other.is_usable());
    }

    #[test]
    fn pool_fills_then_reuses_and_replaces() {
        let pool = SessionPool::new(SessionPoolOptions { max_pool_size: 2, ..Default::default() }, None);
        let a = pool.get_session();
        let b = pool.get_session();
        assert_ne!(a.id(), b.id());
        for _ in 0..10 {
            let s = pool.get_session();
            assert!(s.id() == a.id() || s.id() == b.id());
        }
        a.retire();
        b.retire();
        let c = pool.get_session();
        assert!(c.id() != a.id() && c.id() != b.id());
        assert_eq!(pool.get_session_by_id(c.id()).unwrap().id(), c.id());
        assert!(pool.get_session_by_id(a.id()).is_none());
    }
}
