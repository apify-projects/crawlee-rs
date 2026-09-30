//! Fixture site for benchmarks: `/p/{i}` for `i` in `0..pages`, each a ~100 KB product listing.
//!
//! Page `i` links to pages `10i+1 ..= 10i+10` (so every page is reachable from `/p/0`) plus a
//! navigation bar of already-seen pages, like a real site where most links are duplicates.
//!
//! Run it as a separate process so that its CPU time is not counted against the crawler:
//!
//! ```sh
//! cargo run --release --example bench_server -- 127.0.0.1:3000 10000 [latency_ms]
//! ```

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::Html;
use axum::routing::get;

struct Site {
    pages: usize,
    latency: Duration,
}

fn render(i: usize, pages: usize) -> String {
    let mut html = String::with_capacity(110_000);
    let _ = write!(
        html,
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"><title>Product listing {i}</title>\
         <link rel=\"stylesheet\" href=\"/static/site.css\"><script>window.__STATE__ = {{\"page\": {i}}};</script></head><body>\
         <header><nav class=\"main-nav\">"
    );
    for nav in 0..40 {
        let _ = write!(html, "<a class=\"nav-link\" href=\"/p/{}\">Category {nav}</a>", nav % pages.max(1));
    }
    html.push_str("</nav></header><main><ul class=\"products\">");
    for p in 0..20 {
        let id = i * 20 + p;
        let _ = write!(
            html,
            "<li class=\"product\" data-id=\"{id}\"><a class=\"product-link\" href=\"/p/{i}?item={id}#details\">\
             <h2 class=\"name\">Product &quot;{id}&quot; &amp; friends</h2></a>\
             <span class=\"price\">{}.{:02} &euro;</span>\
             <p class=\"description\">{}</p></li>",
            10 + id % 90,
            id % 100,
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ".repeat(20)
        );
    }
    html.push_str("</ul><nav class=\"pagination\">");
    for k in 1..=10 {
        let next = i * 10 + k;
        if next < pages {
            let _ = write!(html, "<a class=\"next\" href=\"/p/{next}\">Page {next}</a>");
        }
    }
    html.push_str("</nav></main><footer>");
    for f in 0..30 {
        let _ = write!(html, "<a href=\"https://external-{f}.example.com/\">Partner {f}</a>");
    }
    html.push_str("</footer></body></html>");
    html
}

async fn page(State(site): State<Arc<Site>>, Path(i): Path<usize>) -> Html<String> {
    if !site.latency.is_zero() {
        tokio::time::sleep(site.latency).await;
    }
    Html(render(i, site.pages))
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:3000".to_owned());
    let pages: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10_000);
    let latency = Duration::from_millis(args.next().and_then(|a| a.parse().ok()).unwrap_or(0));

    println!("page size: {} bytes", render(1, pages).len());
    let site = Arc::new(Site { pages, latency });
    let app = axum::Router::new().route("/p/{i}", get(page)).with_state(site);
    let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("serving {pages} pages on http://{addr} (latency {latency:?})");
    axum::serve(listener, app).await.expect("serve");
}
