//! [`Transport`] built on `reqwest` (hyper + rustls).
//!
//! One `reqwest::Client` is kept per (proxy, TLS verification) combination, so connections,
//! TLS sessions and HTTP/2 streams are reused across requests instead of being set up per call.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use http::HeaderValue;
use http::header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use parking_lot::Mutex;

use crate::{HttpClientError, HttpRequest, HttpResponse, Transport, TransportOptions};

/// User agent sent when the request does not set one.
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";

type ClientKey = (Option<String>, bool);

/// `reqwest`-based transport with transparent gzip, brotli and deflate decompression.
pub struct ReqwestTransport {
    clients: Mutex<HashMap<ClientKey, reqwest::Client>>,
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
}

impl std::fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestTransport").finish_non_exhaustive()
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl ReqwestTransport {
    pub fn new() -> Self {
        ReqwestTransport {
            clients: Mutex::new(HashMap::new()),
            connect_timeout: Duration::from_secs(30),
            pool_idle_timeout: Duration::from_secs(90),
        }
    }

    fn client(&self, options: &TransportOptions) -> Result<reqwest::Client, HttpClientError> {
        let key = (options.proxy_url.as_ref().map(|u| u.to_string()), options.ignore_tls_errors);
        if let Some(client) = self.clients.lock().get(&key) {
            return Ok(client.clone());
        }

        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(self.connect_timeout)
            .pool_idle_timeout(self.pool_idle_timeout)
            .tcp_nodelay(true)
            .tls_danger_accept_invalid_certs(options.ignore_tls_errors);
        // Without an explicit proxy, the system proxy settings (`HTTPS_PROXY`, `NO_PROXY`, ...)
        // apply, as with curl.
        if let Some(proxy) = &options.proxy_url {
            builder = builder
                .proxy(reqwest::Proxy::all(proxy.as_str()).map_err(|err| HttpClientError::Proxy(err.to_string()))?);
        }
        let client = builder.build().map_err(|err| HttpClientError::Other(err.to_string()))?;

        Ok(self.clients.lock().entry(key).or_insert(client).clone())
    }
}

fn map_error(err: reqwest::Error, via_proxy: bool) -> HttpClientError {
    let message = error_chain(&err);
    if via_proxy && (err.is_connect() || message.to_ascii_lowercase().contains("proxy")) {
        HttpClientError::Proxy(message)
    } else if err.is_connect() {
        HttpClientError::Connect(message)
    } else {
        HttpClientError::Other(message)
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn fetch(&self, request: HttpRequest, options: &TransportOptions) -> Result<HttpResponse, HttpClientError> {
        let client = self.client(options)?;
        let via_proxy = options.proxy_url.is_some();

        let mut headers = request.headers;
        headers.entry(USER_AGENT).or_insert(HeaderValue::from_static(DEFAULT_USER_AGENT));
        headers
            .entry(ACCEPT)
            .or_insert(HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"));
        headers.entry(ACCEPT_LANGUAGE).or_insert(HeaderValue::from_static("en-US,en;q=0.9"));

        let mut builder = client.request(request.method, request.url.as_str()).headers(headers);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }

        let mut response = builder.send().await.map_err(|err| map_error(err, via_proxy))?;

        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let url = response.url().clone();

        let body = match options.max_body_size {
            None => response.bytes().await.map_err(|err| map_error(err, via_proxy))?,
            Some(limit) => {
                if response.content_length().is_some_and(|len| len as usize > limit) {
                    return Err(HttpClientError::BodyTooLarge { limit });
                }
                let mut buffer = BytesMut::new();
                while let Some(chunk) = response.chunk().await.map_err(|err| map_error(err, via_proxy))? {
                    if buffer.len() + chunk.len() > limit {
                        return Err(HttpClientError::BodyTooLarge { limit });
                    }
                    buffer.extend_from_slice(&chunk);
                }
                Bytes::from(buffer)
            }
        };

        Ok(HttpResponse { status, headers, url, body, redirects: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HttpClient, RedirectingClient, SendOptions};
    use axum::Router;
    use axum::response::{IntoResponse, Redirect};
    use axum::routing::get;
    use url::Url;

    async fn serve() -> Url {
        let app = Router::new()
            .route("/", get(|| async { Redirect::to("/landing") }))
            .route(
                "/landing",
                get(|headers: http::HeaderMap| async move {
                    let ua = headers.get(USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
                    ([("set-cookie", "visited=yes")], format!("ua={ua}")).into_response()
                }),
            )
            .route("/big", get(|| async { "x".repeat(10_000) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Url::parse(&format!("http://{addr}/")).unwrap()
    }

    #[tokio::test]
    async fn real_http_roundtrip() {
        let base = serve().await;
        let client = RedirectingClient::new(ReqwestTransport::new());
        let jar = std::sync::Arc::new(crate::CookieJar::new());

        let response = client
            .send_request(
                HttpRequest::get(base.clone()),
                &SendOptions { cookie_jar: Some(jar.clone()), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.url.path(), "/landing");
        assert_eq!(response.body, Bytes::from(format!("ua={DEFAULT_USER_AGENT}")));
        assert_eq!(jar.cookie_header(&base).as_deref(), Some("visited=yes"));

        let err = client
            .send_request(
                HttpRequest::get(base.join("/big").unwrap()),
                &SendOptions { max_body_size: Some(1000), ..Default::default() },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, HttpClientError::BodyTooLarge { limit: 1000 }));
    }
}
