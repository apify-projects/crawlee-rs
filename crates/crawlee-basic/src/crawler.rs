//! [`BasicCrawler`]: the task loop, retries and session handling every crawler builds on.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use tokio::sync::{Notify, OnceCell, Semaphore};
use tokio::task::JoinSet;
use tracing::Instrument as _;

use crawlee_core::storage::backend::BatchAddRequestsResult;
use crawlee_core::{Dataset, KeyValueStore, Request, RequestManager, Services, StorageIdentifier};
use crawlee_http_client::HttpClient;

use crate::context::{BasicContext, CrawlerShared, CrawlingContext, RequestScope};
use crate::errors::{ErrorKind, HandlerPanic, RequestHandlerTimeout, error_message};
use crate::handler::{ErrorHandler, Identity, Middleware, RequestHandler};
use crate::proxy::ProxySource;
use crate::session::{Session, SessionPool, SessionPoolOptions};
use crate::statistics::{FinalStatistics, Statistics};

/// How long the task loop waits for new requests before checking the queue again.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Crawler settings, with the defaults of Crawlee for JS where they exist.
#[derive(Clone, Debug)]
pub struct CrawlerOptions {
    /// Maximum number of requests processed in parallel.
    ///
    /// This milestone runs a fixed-size pool; autoscaling by system load (`ConcurrencySystem` in
    /// Crawlee for JS) is planned.
    pub max_concurrency: usize,
    /// Retries per request after the first attempt (`maxRequestRetries`).
    pub max_request_retries: u32,
    /// Stop after this many requests finished (`maxRequestsPerCrawl`).
    pub max_requests_per_crawl: Option<u64>,
    /// Links deeper than this are not enqueued (`maxCrawlDepth`).
    pub max_crawl_depth: Option<u32>,
    /// Timeout of the request handler alone (`requestHandlerTimeoutSecs`).
    pub request_handler_timeout: Duration,
    /// Timeout of `send_request` calls made from handlers.
    pub send_request_timeout: Duration,
    pub session_pool: SessionPoolOptions,
}

impl Default for CrawlerOptions {
    fn default() -> Self {
        CrawlerOptions {
            max_concurrency: 50,
            max_request_retries: 3,
            max_requests_per_crawl: None,
            max_crawl_depth: None,
            request_handler_timeout: Duration::from_secs(60),
            send_request_timeout: Duration::from_secs(30),
            session_pool: SessionPoolOptions::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("a request handler is required: call `request_handler()` or `router()`")]
    MissingRequestHandler,
    #[error("max_concurrency must be at least 1")]
    ZeroConcurrency,
}

struct Inner<P: Middleware<BasicContext>> {
    pipeline: P,
    handler: Arc<dyn RequestHandler<P::Out>>,
    error_handler: Option<Arc<dyn ErrorHandler>>,
    failed_request_handler: Option<Arc<dyn ErrorHandler>>,
    options: CrawlerOptions,
    services: Services,
    request_manager: Option<Arc<dyn RequestManager>>,
    http_client: Arc<dyn HttpClient>,
    session_pool: SessionPool,
    statistics: Arc<Statistics>,
    shared: OnceCell<Arc<CrawlerShared>>,
}

/// A crawler that runs a handler for every request of its queue, with retries, sessions and
/// concurrency control, but without fetching anything itself. [`HttpCrawler`] and friends in
/// `crawlee-http` are `BasicCrawler`s with a pipeline that fetches and parses the page.
///
/// ```no_run
/// use crawlee_basic::{BasicContext, BasicCrawler};
///
/// # async fn run() -> anyhow::Result<()> {
/// let crawler = BasicCrawler::builder()
///     .request_handler(|ctx: BasicContext| async move {
///         ctx.push_data(&serde_json::json!({ "url": ctx.request().url }))?;
///         Ok(())
///     })
///     .build()?;
/// let stats = crawler.run(["https://crawlee.dev"]).await?;
/// println!("{} requests succeeded", stats.requests_succeeded);
/// # Ok(()) }
/// ```
///
/// [`HttpCrawler`]: https://docs.rs/crawlee-http
pub struct BasicCrawler<P: Middleware<BasicContext> = Identity> {
    inner: Arc<Inner<P>>,
}

impl<P: Middleware<BasicContext>> Clone for BasicCrawler<P> {
    fn clone(&self) -> Self {
        BasicCrawler { inner: self.inner.clone() }
    }
}

impl BasicCrawler<Identity> {
    pub fn builder() -> BasicCrawlerBuilder<Identity> {
        BasicCrawlerBuilder::with_pipeline(Identity)
    }
}

/// Builder of [`BasicCrawler`]. Crawlers built on `BasicCrawler` start from
/// [`BasicCrawlerBuilder::with_pipeline`].
#[must_use]
pub struct BasicCrawlerBuilder<P: Middleware<BasicContext>> {
    pipeline: P,
    handler: Option<Arc<dyn RequestHandler<P::Out>>>,
    error_handler: Option<Arc<dyn ErrorHandler>>,
    failed_request_handler: Option<Arc<dyn ErrorHandler>>,
    options: CrawlerOptions,
    services: Option<Services>,
    request_manager: Option<Arc<dyn RequestManager>>,
    http_client: Option<Arc<dyn HttpClient>>,
    proxies: Option<Arc<dyn ProxySource>>,
}

impl<P> BasicCrawlerBuilder<P>
where
    P: Middleware<BasicContext>,
    P::Out: CrawlingContext,
{
    pub fn with_pipeline(pipeline: P) -> Self {
        BasicCrawlerBuilder {
            pipeline,
            handler: None,
            error_handler: None,
            failed_request_handler: None,
            options: CrawlerOptions::default(),
            services: None,
            request_manager: None,
            http_client: None,
            proxies: None,
        }
    }

    /// The handler run for every request. A [`Router`](crate::Router) is a handler too.
    pub fn request_handler(mut self, handler: impl RequestHandler<P::Out>) -> Self {
        self.handler = Some(Arc::new(handler));
        self
    }

    /// Alias of [`request_handler`](Self::request_handler) for routers.
    pub fn router(self, router: crate::Router<P::Out>) -> Self {
        self.request_handler(router)
    }

    /// Called before each retry of a failed request.
    pub fn error_handler(mut self, handler: impl ErrorHandler) -> Self {
        self.error_handler = Some(Arc::new(handler));
        self
    }

    /// Called once a request failed for the last time.
    pub fn failed_request_handler(mut self, handler: impl ErrorHandler) -> Self {
        self.failed_request_handler = Some(Arc::new(handler));
        self
    }

    pub fn options(mut self, options: CrawlerOptions) -> Self {
        self.options = options;
        self
    }

    pub fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.options.max_concurrency = max_concurrency;
        self
    }

    pub fn max_request_retries(mut self, retries: u32) -> Self {
        self.options.max_request_retries = retries;
        self
    }

    pub fn max_requests_per_crawl(mut self, max: u64) -> Self {
        self.options.max_requests_per_crawl = Some(max);
        self
    }

    pub fn max_crawl_depth(mut self, depth: u32) -> Self {
        self.options.max_crawl_depth = Some(depth);
        self
    }

    pub fn request_handler_timeout(mut self, timeout: Duration) -> Self {
        self.options.request_handler_timeout = timeout;
        self
    }

    pub fn session_pool_options(mut self, options: SessionPoolOptions) -> Self {
        self.options.session_pool = options;
        self
    }

    /// Configuration and storage of this crawler. Defaults to [`Services::global`]: storages
    /// under `./storage` (or `CRAWLEE_STORAGE_DIR`), purged when the first crawler starts.
    pub fn services(mut self, services: Services) -> Self {
        self.services = Some(services);
        self
    }

    /// Where requests come from. Defaults to the default request queue of the services.
    pub fn request_manager(mut self, manager: Arc<dyn RequestManager>) -> Self {
        self.request_manager = Some(manager);
        self
    }

    pub fn http_client(mut self, client: Arc<dyn HttpClient>) -> Self {
        self.http_client = Some(client);
        self
    }

    /// Proxies for new sessions, e.g. a [`ProxyConfiguration`](crate::ProxyConfiguration).
    pub fn proxy_configuration(mut self, proxies: Arc<dyn ProxySource>) -> Self {
        self.proxies = Some(proxies);
        self
    }

    pub fn build(self) -> Result<BasicCrawler<P>, BuildError> {
        let handler = self.handler.ok_or(BuildError::MissingRequestHandler)?;
        if self.options.max_concurrency == 0 {
            return Err(BuildError::ZeroConcurrency);
        }
        let session_pool = SessionPool::new(self.options.session_pool.clone(), self.proxies);
        Ok(BasicCrawler {
            inner: Arc::new(Inner {
                pipeline: self.pipeline,
                handler,
                error_handler: self.error_handler,
                failed_request_handler: self.failed_request_handler,
                options: self.options,
                services: self.services.unwrap_or_else(|| Services::global().clone()),
                request_manager: self.request_manager,
                http_client: self.http_client.unwrap_or_else(crawlee_http_client::default_client),
                session_pool,
                statistics: Arc::new(Statistics::new()),
                shared: OnceCell::new(),
            }),
        })
    }
}

impl<P> BasicCrawler<P>
where
    P: Middleware<BasicContext>,
    P::Out: CrawlingContext,
{
    async fn shared(&self) -> anyhow::Result<&Arc<CrawlerShared>> {
        let inner = &self.inner;
        inner
            .shared
            .get_or_try_init(|| async {
                // Before anything is opened, so the storages this crawler uses start empty.
                inner.services.purge_on_start().await?;
                let request_manager: Arc<dyn RequestManager> = match &inner.request_manager {
                    Some(manager) => manager.clone(),
                    None => Arc::new(inner.services.open_request_queue(&StorageIdentifier::Default).await?),
                };
                let dataset = inner.services.open_dataset(&StorageIdentifier::Default).await?;
                let key_value_store = inner.services.open_key_value_store(&StorageIdentifier::Default).await?;
                request_manager.set_expected_request_processing_time(inner.options.request_handler_timeout * 2).await?;
                Ok::<_, anyhow::Error>(Arc::new(CrawlerShared {
                    services: inner.services.clone(),
                    request_manager,
                    dataset,
                    key_value_store,
                    http_client: inner.http_client.clone(),
                    statistics: inner.statistics.clone(),
                    max_crawl_depth: inner.options.max_crawl_depth,
                    send_request_timeout: inner.options.send_request_timeout,
                    requests_added: Notify::new(),
                }))
            })
            .await
    }

    /// Adds requests to the crawler's queue.
    pub async fn add_requests<I, R>(&self, requests: I) -> anyhow::Result<BatchAddRequestsResult>
    where
        I: IntoIterator<Item = R>,
        R: Into<Request>,
    {
        let shared = self.shared().await?;
        let requests: Vec<Request> = requests.into_iter().map(Into::into).collect();
        let result = shared.request_manager.add_requests(requests, false).await?;
        shared.requests_added.notify_one();
        Ok(result)
    }

    pub fn statistics(&self) -> &Statistics {
        &self.inner.statistics
    }

    pub async fn dataset(&self) -> anyhow::Result<Dataset> {
        Ok(self.shared().await?.dataset.clone())
    }

    pub async fn key_value_store(&self) -> anyhow::Result<KeyValueStore> {
        Ok(self.shared().await?.key_value_store.clone())
    }

    pub async fn request_manager(&self) -> anyhow::Result<Arc<dyn RequestManager>> {
        Ok(self.shared().await?.request_manager.clone())
    }

    /// Writes the default dataset to a file, like `crawler.exportData()` in Crawlee for JS: a
    /// JSON array for `.json`, one object per line for `.jsonl`. Returns the number of items.
    pub async fn export_data(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<usize> {
        let path = path.as_ref();
        let items: Vec<serde_json::Value> = self.dataset().await?.get_all().await?;

        let content = match path.extension().and_then(|ext| ext.to_str()) {
            Some("json") => {
                let mut text = serde_json::to_string_pretty(&items)?;
                text.push('\n');
                text
            }
            Some("jsonl") => {
                let mut text = String::new();
                for item in &items {
                    text.push_str(&serde_json::to_string(item)?);
                    text.push('\n');
                }
                text
            }
            _ => anyhow::bail!("unsupported export format of '{}': use a .json or .jsonl file", path.display()),
        };

        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, content).await?;
        Ok(items.len())
    }

    /// Adds `requests` and crawls until the queue is finished (or a limit is reached).
    ///
    /// Returns an error only for a critical failure (a [`CriticalError`], a missing route or a
    /// storage failure); failed requests are reported in the statistics.
    ///
    /// [`CriticalError`]: crawlee_core::CriticalError
    pub async fn run<I, R>(&self, requests: I) -> anyhow::Result<FinalStatistics>
    where
        I: IntoIterator<Item = R>,
        R: Into<Request>,
    {
        let shared = self.shared().await?.clone();
        self.add_requests(requests).await?;
        self.inner.statistics.start();
        tracing::info!(max_concurrency = self.inner.options.max_concurrency, "Starting the crawler.");

        let mut result = self.task_loop(&shared).await;
        self.inner.statistics.finish();
        if let Err(err) = self.inner.services.storage.teardown().await {
            tracing::warn!("Failed to tear down the storage: {err}");
            result = result.and(Err(err.into()));
        }
        let stats = self.inner.statistics.snapshot();

        match result {
            Ok(()) => {
                tracing::info!(
                    requests_succeeded = stats.requests_succeeded,
                    requests_failed = stats.requests_failed,
                    requests_retries = stats.requests_retries,
                    crawler_runtime_millis = stats.crawler_runtime_millis,
                    "Crawler finished."
                );
                Ok(stats)
            }
            Err(err) => {
                tracing::error!("Crawler stopped by a critical error: {err:#}");
                Err(err)
            }
        }
    }

    async fn task_loop(&self, shared: &Arc<CrawlerShared>) -> anyhow::Result<()> {
        let options = &self.inner.options;
        let permits = Arc::new(Semaphore::new(options.max_concurrency));
        let mut tasks: JoinSet<anyhow::Result<()>> = JoinSet::new();
        let mut limit_logged = false;

        loop {
            while let Some(joined) = tasks.try_join_next() {
                Self::check_task(joined, &mut tasks)?;
            }

            if let Some(max) = options.max_requests_per_crawl {
                let in_flight = tasks.len() as u64;
                if self.inner.statistics.requests_finished() + in_flight >= max {
                    if !limit_logged {
                        tracing::info!(
                            max,
                            "Crawler reached the maxRequestsPerCrawl limit; waiting for running requests."
                        );
                        limit_logged = true;
                    }
                    match tasks.join_next().await {
                        Some(joined) => {
                            Self::check_task(joined, &mut tasks)?;
                            continue;
                        }
                        None => return Ok(()),
                    }
                }
            }

            let permit = tokio::select! {
                permit = permits.clone().acquire_owned() => permit.expect("the semaphore is never closed"),
                Some(joined) = tasks.join_next(), if !tasks.is_empty() && permits.available_permits() == 0 => {
                    Self::check_task(joined, &mut tasks)?;
                    continue;
                }
            };

            match shared.request_manager.fetch_next_request().await? {
                Some(request) => {
                    let crawler = self.clone();
                    let shared = shared.clone();
                    let span = tracing::info_span!("request", url = %request.url);
                    tasks.spawn(
                        async move {
                            let _permit = permit;
                            crawler.process_request(shared, request).await
                        }
                        .instrument(span),
                    );
                }
                None => {
                    drop(permit);
                    if tasks.is_empty() && shared.request_manager.is_finished().await? {
                        return Ok(());
                    }
                    let added = shared.requests_added.notified();
                    tokio::select! {
                        Some(joined) = tasks.join_next(), if !tasks.is_empty() => Self::check_task(joined, &mut tasks)?,
                        () = added => {}
                        () = tokio::time::sleep(IDLE_POLL_INTERVAL) => {}
                    }
                }
            }
        }
    }

    fn check_task(
        joined: Result<anyhow::Result<()>, tokio::task::JoinError>,
        tasks: &mut JoinSet<anyhow::Result<()>>,
    ) -> anyhow::Result<()> {
        match joined {
            Ok(Ok(())) => Ok(()),
            Ok(Err(critical)) => {
                tasks.abort_all();
                Err(critical)
            }
            Err(join_error) if join_error.is_cancelled() => Ok(()),
            Err(join_error) => {
                tasks.abort_all();
                Err(anyhow::anyhow!("request task failed: {join_error}"))
            }
        }
    }

    fn session_for(&self, request: &Request) -> Arc<Session> {
        request
            .session_id()
            .and_then(|id| self.inner.session_pool.get_session_by_id(id))
            .unwrap_or_else(|| self.inner.session_pool.get_session())
    }

    /// Processes one request. Returns `Err` only for errors that must stop the crawl.
    async fn process_request(&self, shared: Arc<CrawlerShared>, request: Request) -> anyhow::Result<()> {
        let started = Instant::now();
        let session = self.session_for(&request);
        let scope = RequestScope::new(shared.clone(), Some(session.clone()));
        // Fallback in case the handler leaks its context into a task that outlives it.
        let snapshot = request.clone();

        let result = self.run_pipeline_and_handler(BasicContext::new(request, scope.clone())).await;
        let mut request = scope.take_returned().unwrap_or(snapshot);

        let result = match result {
            Ok(()) => scope.transaction.commit().await.map_err(anyhow::Error::from),
            Err(err) => {
                scope.transaction.rollback();
                Err(err)
            }
        };

        match result {
            Ok(()) => {
                shared.request_manager.mark_request_as_handled(&mut request).await?;
                session.mark_good();
                shared.statistics.record_success(started.elapsed(), request.retry_count);
                Ok(())
            }
            Err(err) => {
                let kind = ErrorKind::of(&err);
                if kind == ErrorKind::Skipped {
                    tracing::debug!("Skipping request: {err:#}");
                    shared.request_manager.mark_request_as_handled(&mut request).await?;
                    shared.statistics.record_skipped();
                    return Ok(());
                }
                self.handle_request_error(&shared, &session, request, err, kind, started).await?;
                if !kind.absolves_session() {
                    session.mark_bad();
                }
                Ok(())
            }
        }
    }

    async fn run_pipeline_and_handler(&self, ctx: BasicContext) -> anyhow::Result<()> {
        let ctx = match AssertUnwindSafe(self.inner.pipeline.run(ctx)).catch_unwind().await {
            Ok(result) => result?,
            Err(panic) => return Err(panic_error(panic)),
        };

        let timeout = self.inner.options.request_handler_timeout;
        match tokio::time::timeout(timeout, AssertUnwindSafe(self.inner.handler.handle(ctx)).catch_unwind()).await {
            Ok(Ok(result)) => result,
            Ok(Err(panic)) => Err(panic_error(panic)),
            Err(_) => Err(RequestHandlerTimeout { secs: timeout.as_secs_f64() }.into()),
        }
    }

    /// The retry logic of `requestFunctionErrorHandler` in Crawlee for JS.
    async fn handle_request_error(
        &self,
        shared: &Arc<CrawlerShared>,
        session: &Arc<Session>,
        mut request: Request,
        error: anyhow::Error,
        kind: ErrorKind,
        started: Instant,
    ) -> anyhow::Result<()> {
        let manager = &shared.request_manager;

        if kind == ErrorKind::Throttled {
            // Never really attempted: no retry is spent and the session is not blamed.
            tracing::debug!("Deferring request because its domain is rate-limiting us. {error:#}");
            manager.reclaim_request(&request, true).await?;
            return Ok(());
        }

        let message = error_message(&error);
        request.push_error_message(message.clone());

        if kind == ErrorKind::Critical {
            return Err(error);
        }

        let error = Arc::new(error);

        if self.can_be_retried(&request, kind) {
            shared.statistics.record_retry(&message);
            if let Some(handler) = &self.inner.error_handler {
                request = self.call_error_handler(handler, shared, session, request, error.clone()).await;
            }
            if kind == ErrorKind::Session {
                session.retire();
            }
            if !request.no_retry {
                request.retry_count += 1;
                tracing::warn!(
                    retry_count = request.retry_count,
                    "Reclaiming failed request back to the queue. {message}"
                );
                manager.reclaim_request(&request, false).await?;
                return Ok(());
            }
        }

        if kind == ErrorKind::Session {
            session.retire();
        }
        shared.statistics.record_error(&message);
        manager.mark_request_as_handled(&mut request).await?;
        shared.statistics.record_failure(started.elapsed(), request.retry_count);
        tracing::error!("Request failed and reached maximum retries. {message}");

        if let Some(handler) = &self.inner.failed_request_handler {
            self.call_error_handler(handler, shared, session, request, error).await;
        }
        Ok(())
    }

    fn can_be_retried(&self, request: &Request, kind: ErrorKind) -> bool {
        if request.no_retry || kind == ErrorKind::NonRetryable {
            return false;
        }
        if kind == ErrorKind::RetryRequest {
            return true;
        }
        request.retry_count < request.max_retries().unwrap_or(self.inner.options.max_request_retries)
    }

    /// Runs an error handler with a fresh context; its storage writes are committed if it
    /// succeeds. Returns the (possibly modified) request.
    async fn call_error_handler(
        &self,
        handler: &Arc<dyn ErrorHandler>,
        shared: &Arc<CrawlerShared>,
        session: &Arc<Session>,
        request: Request,
        error: Arc<anyhow::Error>,
    ) -> Request {
        let scope = RequestScope::new(shared.clone(), Some(session.clone()));
        let snapshot = request.clone();
        let outcome =
            AssertUnwindSafe(handler.handle(BasicContext::new(request, scope.clone()), error)).catch_unwind().await;
        match outcome {
            Ok(Ok(())) => {
                if let Err(err) = scope.transaction.commit().await {
                    tracing::error!("Committing the storage writes of an error handler failed: {err}");
                }
            }
            Ok(Err(err)) => tracing::error!("Error handler failed: {err:#}"),
            Err(panic) => tracing::error!("Error handler panicked: {:#}", panic_error(panic)),
        }
        scope.take_returned().unwrap_or(snapshot)
    }
}

fn panic_error(panic: Box<dyn std::any::Any + Send>) -> anyhow::Error {
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_owned());
    HandlerPanic { message }.into()
}
