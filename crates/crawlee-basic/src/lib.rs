//! `BasicCrawler` for crawlee-rs: the task loop, retries, sessions, router and statistics every
//! crawler builds on.

pub mod context;
pub mod crawler;
pub mod enqueue;
pub mod errors;
pub mod handler;
pub mod proxy;
pub mod router;
pub mod session;
pub mod statistics;

pub use crate::context::{BasicContext, CrawlingContext};
pub use crate::crawler::{BasicCrawler, BasicCrawlerBuilder, BuildError, CrawlerOptions};
pub use crate::enqueue::{EnqueueLinksOptions, EnqueueLinksResult, SkipReason};
pub use crate::errors::{ErrorKind, MissingRouteError, RequestSkipped};
pub use crate::handler::{ErrorHandler, FnMiddleware, Identity, Middleware, RequestHandler, Then};
pub use crate::proxy::{ProxyConfiguration, ProxyInfo, ProxySource};
pub use crate::router::Router;
pub use crate::session::{Session, SessionOptions, SessionPool, SessionPoolOptions};
pub use crate::statistics::{FinalStatistics, Statistics};
