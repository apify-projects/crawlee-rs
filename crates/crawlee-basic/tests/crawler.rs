//! Behavior of the task loop and the retry logic, mirroring the semantics of Crawlee for JS.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crawlee_basic::{BasicContext, BasicCrawler, Router};
use crawlee_core::errors::{CriticalError, NonRetryableError, RetryRequestError, SessionError};
use crawlee_core::{Configuration, Request, Services};

fn services() -> Services {
    Services::in_memory()
}

async fn items(crawler: &BasicCrawler) -> Vec<Value> {
    crawler.dataset().await.unwrap().get_all().await.unwrap()
}

#[tokio::test]
async fn handles_every_request_once() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let crawler = BasicCrawler::builder()
        .services(services())
        .request_handler(move |ctx: BasicContext| {
            let log = log.clone();
            async move {
                log.lock().push(ctx.request().url.clone());
                ctx.push_data(&json!({ "url": ctx.request().url }))?;
                Ok(())
            }
        })
        .build()
        .unwrap();

    let stats =
        crawler.run(["https://a.dev/1", "https://a.dev/2", "https://a.dev/2/", "https://a.dev/3"]).await.unwrap();

    assert_eq!(stats.requests_succeeded, 3, "the trailing-slash duplicate is deduplicated");
    assert_eq!(stats.requests_failed, 0);
    let mut seen = seen.lock().clone();
    seen.sort();
    assert_eq!(seen, ["https://a.dev/1", "https://a.dev/2", "https://a.dev/3"]);
    assert_eq!(items(&crawler).await.len(), 3);
}

#[tokio::test]
async fn retries_then_fails_and_rolls_back_writes() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(Mutex::new(None::<Request>));
    let counter = attempts.clone();
    let failed_slot = failed.clone();

    let crawler = BasicCrawler::builder()
        .services(services())
        .max_request_retries(2)
        .request_handler(move |ctx: BasicContext| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                ctx.push_data(&json!({ "never": "committed" }))?;
                anyhow::bail!("boom {}", ctx.request().retry_count)
            }
        })
        .failed_request_handler(move |ctx: BasicContext, error: Arc<anyhow::Error>| {
            let failed_slot = failed_slot.clone();
            async move {
                ctx.push_data(&json!({ "failed": ctx.request().url, "error": error.to_string() }))?;
                *failed_slot.lock() = Some(ctx.request().clone());
                Ok(())
            }
        })
        .build()
        .unwrap();

    let stats = crawler.run(["https://a.dev/x"]).await.unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 3, "1 attempt + 2 retries");
    assert_eq!(stats.requests_failed, 1);
    assert_eq!(stats.requests_retries, 2);
    assert_eq!(stats.retry_histogram, vec![0, 0, 1]);

    let failed = failed.lock().clone().unwrap();
    assert_eq!(failed.retry_count, 2);
    assert_eq!(failed.error_messages, ["boom 0", "boom 1", "boom 2"]);
    assert!(failed.handled_at.is_some());

    // Only the failed-request handler's write survives.
    assert_eq!(items(&crawler).await, vec![json!({ "failed": "https://a.dev/x", "error": "boom 2" })]);
}

#[tokio::test]
async fn error_handler_changes_are_kept_for_the_retry() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .request_handler(|ctx: BasicContext| async move {
            if ctx.request().user_data.get("fixed").is_none() {
                anyhow::bail!("needs fixing");
            }
            ctx.push_data(&json!({ "ok": true }))?;
            Ok(())
        })
        .error_handler(|mut ctx: BasicContext, _error: Arc<anyhow::Error>| async move {
            ctx.request_mut().user_data.insert("fixed".into(), json!(true));
            Ok(())
        })
        .build()
        .unwrap();

    let stats = crawler.run(["https://a.dev/"]).await.unwrap();
    assert_eq!((stats.requests_succeeded, stats.requests_retries), (1, 1));
}

#[tokio::test]
async fn marker_errors_steer_retries() {
    let attempts: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
    let log = attempts.clone();

    let crawler = BasicCrawler::builder()
        .services(services())
        .max_request_retries(1)
        .request_handler(move |ctx: BasicContext| {
            let log = log.clone();
            async move {
                let path = url::Url::parse(&ctx.request().url)?.path().to_owned();
                let attempt = {
                    let mut log = log.lock();
                    let entry = log.entry(path.clone()).or_default();
                    *entry += 1;
                    *entry
                };
                match path.as_str() {
                    "/non-retryable" => Err(NonRetryableError::new("gone").into()),
                    // Retried beyond max_request_retries until it succeeds.
                    "/retry" if attempt < 4 => Err(RetryRequestError::new("again").into()),
                    "/session" => Err(SessionError::new("blocked").into()),
                    _ => Ok(()),
                }
            }
        })
        .build()
        .unwrap();

    let stats =
        crawler.run(["https://a.dev/non-retryable", "https://a.dev/retry", "https://a.dev/session"]).await.unwrap();

    let attempts = attempts.lock();
    assert_eq!(attempts["/non-retryable"], 1);
    assert_eq!(attempts["/retry"], 4);
    assert_eq!(attempts["/session"], 2);
    assert_eq!(stats.requests_succeeded, 1);
    assert_eq!(stats.requests_failed, 2);
}

#[tokio::test]
async fn session_errors_retire_the_session() {
    let sessions = Arc::new(Mutex::new(Vec::new()));
    let log = sessions.clone();
    let crawler = BasicCrawler::builder()
        .services(services())
        .max_concurrency(1)
        .request_handler(move |ctx: BasicContext| {
            let log = log.clone();
            async move {
                let session = ctx.session().unwrap();
                log.lock().push(session.id().to_owned());
                if ctx.request().retry_count == 0 {
                    return Err(SessionError::new("blocked").into());
                }
                assert!(!session.is_retired());
                Ok(())
            }
        })
        .build()
        .unwrap();

    crawler.run(["https://a.dev/"]).await.unwrap();
    let sessions = sessions.lock();
    assert_eq!(sessions.len(), 2);
    assert_ne!(sessions[0], sessions[1], "the retry runs with a fresh session");
}

#[tokio::test]
async fn critical_errors_and_missing_routes_stop_the_crawl() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .request_handler(|_ctx: BasicContext| async move { Err(CriticalError::new("stop everything").into()) })
        .build()
        .unwrap();
    let err = crawler.run(["https://a.dev/"]).await.unwrap_err();
    assert_eq!(err.to_string(), "stop everything");

    let mut router = Router::<BasicContext>::new();
    router.add_handler("KNOWN", |_ctx: BasicContext| async move { Ok(()) });
    let crawler = BasicCrawler::builder().services(services()).router(router).build().unwrap();
    let request = Request::builder("https://a.dev/").label("UNKNOWN").build().unwrap();
    let err = crawler.run([request]).await.unwrap_err();
    assert!(err.to_string().contains("Route not found for label 'UNKNOWN'"), "{err}");
}

#[tokio::test]
async fn router_dispatches_by_label_and_enqueues_deeper() {
    #[derive(serde::Deserialize)]
    struct Detail {
        id: u32,
    }

    let mut router = Router::<BasicContext>::new();
    router.add_default_handler(|ctx: BasicContext| async move {
        let requests = (1..=3).map(|id| {
            Request::builder(format!("https://a.dev/item/{id}"))
                .label("DETAIL")
                .user_data(&json!({ "id": id }))
                .unwrap()
                .build()
                .unwrap()
        });
        ctx.add_requests(requests).await?;
        Ok(())
    });
    router.add_typed_handler("DETAIL", |ctx: BasicContext, data: Detail| async move {
        ctx.push_data(&json!({ "id": data.id, "depth": ctx.request().crawl_depth() }))?;
        Ok(())
    });

    let crawler = BasicCrawler::builder().services(services()).router(router).build().unwrap();
    let stats = crawler.run(["https://a.dev/"]).await.unwrap();
    assert_eq!(stats.requests_succeeded, 4);

    let mut items = items(&crawler).await;
    items.sort_by_key(|item| item["id"].as_u64());
    assert_eq!(
        items,
        vec![json!({ "id": 1, "depth": 1 }), json!({ "id": 2, "depth": 1 }), json!({ "id": 3, "depth": 1 })]
    );
}

#[tokio::test]
async fn max_requests_per_crawl_is_respected() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .max_concurrency(4)
        .max_requests_per_crawl(5)
        .request_handler(|_ctx: BasicContext| async move { Ok(()) })
        .build()
        .unwrap();
    let urls: Vec<String> = (0..20).map(|i| format!("https://a.dev/{i}")).collect();
    let stats = crawler.run(urls).await.unwrap();
    assert_eq!(stats.requests_succeeded, 5);
}

#[tokio::test]
async fn handler_timeouts_and_panics_are_request_errors() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .max_request_retries(0)
        .request_handler_timeout(Duration::from_millis(50))
        .request_handler(|ctx: BasicContext| async move {
            if ctx.request().url.ends_with("slow") {
                tokio::time::sleep(Duration::from_secs(10)).await;
            } else {
                panic!("handler bug");
            }
            Ok(())
        })
        .failed_request_handler(|ctx: BasicContext, _e: Arc<anyhow::Error>| async move {
            ctx.push_data(&json!({ "url": ctx.request().url, "error": ctx.request().error_messages[0] }))?;
            Ok(())
        })
        .build()
        .unwrap();

    let stats = crawler.run(["https://a.dev/slow", "https://a.dev/panic"]).await.unwrap();
    assert_eq!(stats.requests_failed, 2);

    let mut items = items(&crawler).await;
    items.sort_by(|a, b| a["url"].as_str().cmp(&b["url"].as_str()));
    assert_eq!(items[0]["error"], "request handler panicked: handler bug");
    assert_eq!(items[1]["error"], "requestHandler timed out after 0.05 seconds.");
}

#[tokio::test]
async fn requests_added_while_running_are_picked_up() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .request_handler(|ctx: BasicContext| async move {
            let depth = ctx.request().crawl_depth();
            ctx.push_data(&json!({ "depth": depth }))?;
            // Each page links to the next level, three levels deep.
            if depth < 3 {
                ctx.add_requests([format!("https://a.dev/level/{}", depth + 1)]).await?;
            }
            Ok(())
        })
        .build()
        .unwrap();
    let stats = crawler.run(["https://a.dev/level/0"]).await.unwrap();
    assert_eq!(stats.requests_succeeded, 4);
    let mut depths: Vec<u64> = items(&crawler).await.iter().map(|i| i["depth"].as_u64().unwrap()).collect();
    depths.sort();
    assert_eq!(depths, [0, 1, 2, 3]);
}

#[tokio::test]
async fn export_data_writes_json_and_jsonl() {
    let crawler = BasicCrawler::builder()
        .services(services())
        .max_concurrency(1)
        .request_handler(|ctx: BasicContext| async move {
            ctx.push_data(&json!({ "url": ctx.request().url }))?;
            Ok(())
        })
        .build()
        .unwrap();
    crawler.run(["https://a.dev/1", "https://a.dev/2"]).await.unwrap();

    let dir = std::env::temp_dir().join(format!("crawlee-export-{}", std::process::id()));
    let json_path = dir.join("nested").join("results.json");
    assert_eq!(crawler.export_data(&json_path).await.unwrap(), 2);
    let exported: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    assert_eq!(exported, vec![json!({ "url": "https://a.dev/1" }), json!({ "url": "https://a.dev/2" })]);

    let jsonl_path = dir.join("results.jsonl");
    crawler.export_data(&jsonl_path).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&jsonl_path).unwrap(),
        "{\"url\":\"https://a.dev/1\"}\n{\"url\":\"https://a.dev/2\"}\n"
    );

    assert!(crawler.export_data(dir.join("results.csv")).await.is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn file_system_storage_is_purged_once_per_process() {
    let dir = tempfile::tempdir().unwrap();
    let configuration = || Configuration { storage_dir: dir.path().to_owned(), ..Configuration::default() };
    let crawl = |services: Services, url: &'static str| async move {
        let crawler = BasicCrawler::builder()
            .services(services)
            .request_handler(|ctx: BasicContext| async move {
                ctx.push_data(&json!({ "url": ctx.request().url }))?;
                Ok(())
            })
            .build()
            .unwrap();
        crawler.run([url]).await.unwrap();
    };
    let files = || {
        let mut names: Vec<String> = std::fs::read_dir(dir.path().join("datasets/default"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };

    let services = Services::from_configuration(configuration());
    crawl(services.clone(), "https://a.dev/1").await;
    assert_eq!(files(), ["000000001.json", "__metadata__.json"]);

    // A second crawler of the same process adds to the storages of the first one.
    crawl(services, "https://a.dev/2").await;
    assert_eq!(files(), ["000000001.json", "000000002.json", "__metadata__.json"]);

    // A new process starts over: the previous run's storages are purged.
    crawl(Services::from_configuration(configuration()), "https://a.dev/1").await;
    assert_eq!(files(), ["000000001.json", "__metadata__.json"]);
    let item: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("datasets/default/000000001.json")).unwrap())
            .unwrap();
    assert_eq!(item, json!({ "url": "https://a.dev/1" }));
}
