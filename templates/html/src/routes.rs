use crawlee::{EnqueueLinksOptions, HtmlContext, Router};

pub fn router(site_pattern: String) -> Router<HtmlContext> {
    let mut router = Router::new();

    router.add_default_handler(move |ctx: HtmlContext| {
        let site_pattern = site_pattern.clone();
        async move {
            tracing::info!("Enqueueing new URLs from {}", ctx.url());
            ctx.enqueue_links(EnqueueLinksOptions::new().include([site_pattern]).label("detail")).await?;
            Ok(())
        }
    });

    router.add_handler("detail", |ctx: HtmlContext| async move {
        // Extraction runs on a background thread; return owned data from the closure.
        let page = ctx
            .with_html(|doc| -> anyhow::Result<serde_json::Value> {
                let description = doc.select_first("meta[name=description]")?.and_then(|meta| meta.attr("content"));
                let headings = doc.select("h1")?.map(|h1| h1.text().trim().to_owned());
                Ok(serde_json::json!({
                    "title": doc.title(),
                    "description": description,
                    "headings": headings,
                }))
            })
            .await??;

        tracing::info!("{} ({})", page["title"].as_str().unwrap_or_default(), ctx.url());
        ctx.push_data(&serde_json::json!({ "url": ctx.url().as_str(), "page": page }))?;
        Ok(())
    });

    router
}
