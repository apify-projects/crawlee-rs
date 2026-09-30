# Plan: Crawlee in Rust (`crawlee-rs`), HTTP-first and performance-driven

> **Implementation notes from milestone 1** (added after the first implementation; the plan below is otherwise unchanged)
>
> - **The `!Send` DOM problem (§4.3) is solved without `unsafe`.** `scraper`'s `atomic` feature makes `Html` `Send`. `HtmlContext::with_html` parses once on the blocking pool and caches the document behind a mutex.
> - **The `impit` crate is not published on crates.io** (the name is a placeholder), so milestone 1 uses a reqwest transport. `impit` becomes a git dependency or waits for a release.
> - **The golden-file oracle runs Crawlee for JS straight from its TypeScript sources**, using Node 22 type stripping plus a resolve hook (`conformance/oracle`). No build step is needed, which keeps level 1 of §7 cheap to maintain.
> - **Two parity bugs were caught by the conformance work before any user saw them:**
>   - links inside `<noscript>`, which htmlparser2 parses and lol_html does not;
>   - `Cookie` header ordering, which must follow RFC 6265 like tough-cookie, while `cookie_store` iterates in hash order.
> - **Benchmark:** at the same concurrency on 4 vCPUs, Rust uses about 10× less CPU per page than `CheerioCrawler` (1.3 ms vs 13.5 ms), and peak memory is about 3× lower. See README.md.

## Context

Crawlee is maintained in TypeScript (this repo, v4) and Python, and both stay maintained long term. **The only reason for a Rust crate is performance**, specifically in the places where crawling spends its CPU:
- the HTTP stack;
- JSON parsing and serialization;
- HTML parsing and extraction.

**Scope of this plan**
- HTTP crawling only: Basic, Http, Html (in place of Cheerio), and a JSON crawler.
- Browser and adaptive crawlers are **out of scope**. The design keeps them possible later (§6).
- There is no Apify integration now. It will come later as a separate **Apify SDK crate**, whose storage sits on the existing Rust `apify-client`, the way the JS SDK sits on `apify-client-js` (§6.3).

**Facts from the repo that shape the plan**
- The v4 architecture already maps well onto Rust:
  - the `ContextPipeline` middleware;
  - the `ServiceLocator`;
  - collaborators behind interfaces;
  - a 4-trait `StorageBackend`;
  - `BaseHttpClient` over standard `Request`/`Response`.
- **Two parts are already Rust through napi-rs**, and Rust can use them directly with no binding layer:
  - `impit`, the default HTTP client;
  - `@crawlee/fs-storage-native`, which owns the on-disk format and request-queue locking.
- In `packages/cheerio-crawler/src/internals/cheerio-parser.ts`, `CheerioCrawler` parses with **htmlparser2**. htmlparser2 is fast but does not follow the HTML5 spec. The mature Rust parser, html5ever, follows the spec, so the two will build different trees from malformed HTML (§7.5).
- Where the JS side does extra copying on hot paths:
  - Transactions copy every pushed item with `structuredClone` (`packages/core/src/storages/transaction.ts:470`).
  - Response bodies cross the napi boundary into a JS `Buffer`, are converted to a `string` (`cheerio-parser.ts`), then parsed again in JS.

---

## 1. Why Rust should be faster, and how we prove it before building everything

**Where the CPU probably goes in a TS HTTP crawl.** These are hypotheses; the Phase 0 profiling confirms or refutes them.

- **HTTP**
  - Fetching itself is already native in impit, but each response is copied into a JS `Buffer` and its headers into a JS `Response`.
  - The cookie jar (tough-cookie), the redirect loop, and the per-request option objects are pure JS.
- **HTML**
  - The `Buffer` is converted to a `string`, then parsed by htmlparser2 into a DOM and wrapped by cheerio.
  - Selectors are matched by css-select, which is JS.
  - Link extraction runs `new URL()` for every link, then `normalizeUrl` for the unique key, then minimatch and regex filters. That can mean thousands of calls per page.
- **JSON**
  - Parsing: `JSON.parse` builds a whole object tree even when the handler needs only three fields.
  - `pushData` checks that items are serializable, then copies them with `structuredClone` into the transaction, then serializes them again for storage.
- **One thread**: user handlers, parsing, and extraction all share one event loop. Crawlee does not use worker threads.

**The Rust design for each hot path**

| Area | Design |
|---|---|
| HTTP | Long-lived clients (impit, or hyper/reqwest) pooled per (proxy, impersonation profile), with keep-alive and HTTP/2 multiplexing. `rustls` with session resumption. A DNS cache (`hickory-resolver`). Streaming decompression. `bytes::Bytes` bodies that are never copied. A `cookie_store` jar per session. Body size limits. |
| Decoding | Check for UTF-8 with `simdutf8` and view it as `&str` with no copy. Other charsets go through `encoding_rs` (SIMD fast paths). Keep the 1024-byte `<meta charset>` prescan. The body stays `Bytes` and a string view is created lazily. |
| JSON | `ctx.json::<T>()` deserializes straight into the user's `serde` struct, without building a tree. This is the biggest structural win over JS. Default `serde_json` (with `preserve_order` wherever a `Value` is used). An optional `simd` feature uses `sonic-rs` or `simd-json`. `RawValue` for pass-through. |
| Dataset writes | `push_data<T: Serialize>` **serializes once, into bytes**. The transaction journal holds those bytes (no `structuredClone`). The file-system backend writes them as they are, and a future Apify backend can batch them into the API request body without re-serializing. |
| HTML, streaming (Tier 1) | `lol_html` pulls `a[href]`/`base[href]` for `enqueue_links` and runs the blocked-page selectors **without building a DOM**, in linear time and small memory. Most crawls get their links this way. |
| HTML, DOM (Tier 2) | Parsed lazily on the first `ctx.html()`, at most once per request. An html5ever-based DOM (candidates: `scraper`, `dom_query`), chosen in the Phase 0 spike. Compiled selectors are cached in an LRU keyed by the selector string. Extraction runs on a blocking/rayon pool (§4.3). |
| URLs and enqueueing | `url`, which follows the WHATWG standard like JS `new URL`, so parity is testable. `normalizeUrl` is ported. Filters are compiled once per call (`globset`, a `RegexSet` that tests every pattern in one pass). The public-suffix list is compiled into the binary (`psl`/`addr`). |
| Request dedup | The deduplication cache (up to 1M entries in TS) stores 128-bit hashes of `uniqueKey` in place of strings. Memory drops by an order of magnitude. |
| Concurrency | A multi-threaded tokio runtime. User handlers and extraction run in parallel on every core, which Node cannot do without workers. |
| Misc | An optional `mimalloc` allocator. A non-blocking `tracing` writer. No log formatting when the level is off. |

**The benchmark must include realistic latency.** On real networks a crawl is often limited by proxies, target rate limits and latency, not by CPU. The fixture server injects latency (for example 50–300 ms) and a range of page sizes, to find the concurrency level at which TS runs out of CPU and Rust doesn't.

**Go/no-go gate (end of Phase 0).** Build a minimal pipeline:
- fetch with impit → decode → link extraction → dedup → `serde` JSON push;
- run it against Crawlee TS `CheerioCrawler` / `HttpCrawler` on the same local fixture corpus.

Measure:
- pages per second per core;
- peak RSS;
- p99 handler latency;
- for JSON crawls, items per second.

Proceed only if the gain clears thresholds the team sets beforehand. A suggested starting point: at least 3× throughput per core, and at least 2× lower memory at equal concurrency.

---

## 2. Target architecture: a Cargo workspace (HTTP scope)

Package boundaries are kept. Optional npm dependencies become Cargo features. The API uses snake_case and follows crawlee-python naming (`push_data`, `enqueue_links`, `max_requests_per_crawl`).

| TS package | Rust crate | Notes |
|---|---|---|
| `@crawlee/types` | `crawlee-core::traits` | `StorageBackend`, `DatasetBackend`, `KeyValueStoreBackend`, `RequestQueueBackend`, `HttpClient`, `SessionPool`, `RequestLoader`, `RequestManager`, `Statistics`, `LoadSignal`, `EventManager` |
| `@crawlee/utils` | `crawlee-utils` | sitemap (`quick-xml` streaming, `flate2`), robots (`texting_robots`), social handles (`regex`), Open Graph, microdata, `html_to_text`, URL strategy |
| `@crawlee/core` | `crawlee-core` | Request, RequestQueue, RequestList, Tandem, Dataset, KeyValueStore and codec, transactions, Configuration, events, ProxyConfiguration, RecoverableState, Services |
| memory backend | `crawlee-storage-memory` | |
| `@crawlee/fs-storage` | `crawlee-storage-fs` | the existing native crate, used directly |
| `@crawlee/basic` | `crawlee-basic` | BasicCrawler, pipeline, Router, sessions, ConcurrencySystem, AutoscaledPool, statistics, error tracker, ThrottlingRequestManager, SitemapRequestLoader, enqueue links |
| `@crawlee/http-client` + `impit-client` | `crawlee-http-client` + `crawlee-impit` (default) and `reqwest` (feature) | `got-scraping` is dropped |
| `@crawlee/http` + `@crawlee/cheerio` | `crawlee-http` + `crawlee-html` | `HttpCrawler`, `JsonCrawler`, `DomCrawler<P: DomParser>`, `HtmlCrawler` |
| `@crawlee/otel` | the `otel` feature (`tracing-opentelemetry`) | spans are built in, with no monkeypatching |
| `@crawlee/cli` + `templates` | `crawlee-cli` (clap) + Rust templates | |
| `crawlee` | a `crawlee` facade crate with features | |
| browser-pool, browser, playwright, puppeteer, stagehand, adaptive | **not now**; extension points in §6 | |

**Baseline:** edition 2024, MSRV 1.88 (let-chains; verified with `cargo +1.88 check`), tokio, and `#![forbid(unsafe_code)]` (§5).

**Repositories**
- `apify/crawlee-rs`.
- A new language-neutral `crawlee-conformance` repo for the specs and the differential harness (§7).

---

## 3. Straightforward parts

These map almost one-to-one onto mature crates. The work is volume, not design.

- **The trait layer** from `@crawlee/types`: `async fn` in traits, with `async_trait` or boxed futures wherever a trait object is needed.
- **Utilities**: sitemap parsing, robots.txt, social-handle regexes (ported verbatim), Open Graph and microdata, `html_to_text`, blocked selectors and proxy error strings, and enqueue strategies (`addr`/`psl`) with globs (`globset`).
- **Requests and storage**
  - `Request` as a serde struct. It must keep the **wire-compatible `userData.__crawlee` bag**.
  - `unique_key`: a port of `normalizeUrl`, plus a SHA-256 hash of the payload.
  - Storage frontends: `Dataset` (as a `Stream`), `KeyValueStore` with its content-type codec (`json5` for parsing), `RequestQueue` (`lru`, the hash-based dedup cache, batched background adds), `RequestList`, and `RequestManagerTandem`.
  - CSV export with `csv`.
- **Configuration**: `serde` with `figment` layering, in the same order: constructor, then `CRAWLEE_*` env vars, then `crawlee.json`, then defaults. It can be extended through a generic `#[serde(flatten)] ext`.
- **Sessions and state**: `RecoverableState<T>`, `ProxyConfiguration`, `Session` / `SessionPool` (`cookie_store`), `Statistics`, and `ErrorTracker`, whose message-placeholder grouping ports as is.
- **Concurrency**: `ConcurrencySystem` (shareable, `Arc` plus atomics), `AutoscaledPool`, `SystemStatus` and `Snapshotter`; memory and CPU readings from `sysinfo` and cgroup files.
- **Request managers**: `ThrottlingRequestManager` (per-domain backoff, `Retry-After`, crawl-delay) and `SitemapRequestLoader`, both self-contained state machines.
- **HTTP**
  - The `HttpClient` trait over `http::Request` / `http::Response` (the Rust counterpart of the standard `Response`).
  - The redirect loop from `base-http-client.ts` ports as is: at most 10 redirects; 303, and 301/302 after POST, become GET; credential headers are dropped across origins.
  - Body decoding and content-type handling from `http-crawler.ts`.
- **Tooling**: CLI (`clap`) and templates; `tracing` spans at the points `@crawlee/otel` patches today.

---

## 4. Challenging parts, and the proposed design

### 4.1 Crawling context and `ContextPipeline`
**The problem:** `context_pipeline.ts` builds `Ctx & Ext` by copying property descriptors onto one mutable object with `Object.defineProperty`. It uses getters, non-configurable pinning, placeholders that throw, and symbol-keyed slots. Rust cannot grow a struct at runtime.

**Design:** a concrete, nested context per layer, plus accessor traits:
```rust
pub struct BasicContext { pub request: CrawlingRequest, pub session: Option<Arc<Session>>,
    pub proxy_info: Option<ProxyInfo>, pub log: Logger, /* services, tx, deadline */ }
pub struct HttpContext<E = ()> { base: BasicContext, pub response: HttpResponse, body: Body, pub ext: E }
pub struct HtmlContext<E = ()> { http: HttpContext<E>, doc: LazyDoc }
// Deref<Target = BasicContext>; traits HasResponse, HasHtml (HasPage reserved, §6.1)
```
- **The pipeline stays**, internal and type-state based. A middleware has the shape `trait Middleware<In> { type Out; async fn run(&self, In, &mut Cleanups) -> Result<Self::Out> }`, and `compose` changes the output type. Cleanups run in reverse order and receive the handler's error, as in TS.
- **`extend_context`** fills a typed `ctx.ext: E` slot rather than merging into the context.
- **Handlers are `AsyncFn(Ctx) -> anyhow::Result<()>`** and take the context by value (it holds `Arc` handles). The future is `Send + 'static`, which avoids higher-ranked lifetime trouble.

### 4.2 Router with typed `userData`
- Labels become an enum with a derive (`#[derive(Routes)]`). `user_data` is deserialized into each route's struct, replacing Standard Schema and zod.
- Enqueueing is then type-checked per route: `.route(Route::Detail { .. })`.
- A router keyed by strings stays available for dynamic cases.

### 4.3 HTML API, the `Send` problem, and where parsing runs
**The facade:** a Crawlee-owned `Html` / `Selection`, so the parser can be swapped the way the TS `DOMParser` interface allows. Its methods are cheerio-like: `select`, `find`, `text`, `attr`, `html`, `parent`, `children`, `first`, `iter`. It supports a small set of jQuery extensions (`:contains`) and documents the rest as unsupported.

**`!Send` DOMs:** html5ever DOMs are usually `!Send` (because of `StrTendril`), so they cannot be held across an `.await` in a `Send` handler, and cannot move between threads. The primary API therefore runs extraction on the parsing pool and returns owned data:
```rust
let item: Product = ctx.with_html(|doc| Product { title: doc.select("h1").text(), .. }).await?;
```
This is also the fastest option, because the CPU work happens on the blocking/rayon pool and the I/O reactor stays free.

Also to evaluate:
- a `Send`-capable DOM crate, or a parser that borrows `&str` (`tl`);
- a synchronous `ctx.html()` that is only available inside `spawn_blocking`.

Never `unsafe impl Send`.

**Parser choice (Phase 0 spike).** Compare html5ever with `scraper`, `dom_query`, and `tl` (lenient, closer to htmlparser2) on speed and tree parity. The `DomParser` trait keeps more than one available.

### 4.4 Engineering the performance itself
- Keep `Bytes` from the socket all the way to storage. Audit every clone and allocation on the hot paths with `dhat` and `cargo flamegraph`.
- Size the parsing pool, and keep CPU work off the tokio workers.
- Cache compiled selectors and filters.
- Run `criterion` micro-benchmarks and a macro benchmark in CI, with regression alerts.

### 4.5 Storage transactions
- The journal holds serialized bytes. Handler-scoped capture uses `tokio::task_local!`.
- **Semantic difference:** a task-local does not follow into `tokio::spawn`ed subtasks, whereas JS AsyncLocalStorage propagates into child async work. Offer `ctx.spawn(..)` to carry the scope along.
- Expose a read-only `TransactionView` (dataset items, KVS writes, enqueued requests) now, because the future adaptive crawler needs it (§6.2).

### 4.6 Services, timeouts, teardown
- **Services:** an explicit `Arc<Services>` in place of the `Proxy` / AsyncLocalStorage service locator. The global uses `OnceLock` and keeps the `ServiceConflictError` semantics.
- **Timeouts:** `tokio::time::timeout`. This gives **real cancellation**, which JS lacks. `extend_timeout` needs a resettable deadline (`Sleep::reset` behind a `watch` channel). Every point where a future can be dropped mid-operation must be audited for cancellation safety.
- **Teardown:** Rust has no async `Drop`, so `await using` / `Symbol.asyncDispose` becomes an explicit `close().await`.

### 4.7 Autoscaling on a multi-threaded runtime
- The event-loop-lag signal becomes a runtime scheduling-lag signal.
- The CPU signal must measure CPU across all cores.
- The default settings need retuning, because true parallelism changes the behavior. Parity is checked by behavior, not by matching numbers (§7).

### 4.8 Cookies and errors
- **Cookies:** `cookie_store` and `tough-cookie` differ at the edges: public-suffix handling, cookie order in the `Cookie` header, lax date parsing. This needs a differential test.
- **Errors:** a `thiserror` `CrawleeError` enum (`Session`, `NonRetryable`, `RetryRequest`, `Critical`, `RequestThrottled`), classified by downcasting. Panics become request failures. Without JS stacks, `ErrorTracker` groups by error type plus the source chain.

---

## 5. Breaking changes and `unsafe`

**Breaking changes compared with the TS API**
1. The context is nested and typed. `extend_context` fills `ctx.ext`. Error handlers get `BasicContext` plus `Option`s for the partly built layers.
2. `$` becomes the `Html` / `Selection` facade, mostly used through `with_html(|doc| ..)`. jQuery pseudo-selectors are gone except for a small set.
3. **Parse trees differ from TS on malformed HTML** (html5ever vs htmlparser2). Rust matches browser (parse5) behavior. This is listed as an explicit difference (§7.5).
4. **JSON edge cases differ:**
   - Integers above 2^53 keep full precision in Rust; JS loses it.
   - `serde_json` rejects lone surrogate escapes that JS accepts.
   - Integer-like keys are reordered first in JS objects, but keep insertion order with `preserve_order`.
5. The router uses enums and serde-typed `user_data`.
6. `Services` is explicit. Teardown is an explicit `close().await`.
7. Transaction capture is task-local (`ctx.spawn`).
8. Dataset items must be `Serialize`, and state must be `Serialize + DeserializeOwned`.
9. Options come from builders, not zod bags. Error-statistics grouping differs.
10. The body is `Bytes` plus a lazy `&str`, not a `string | Buffer` union.
11. Removed: the `got-scraping` client and OTel `customInstrumentation`.

**`unsafe` policy:** `#![forbid(unsafe_code)]` in all of our crates, checked in CI. The pressure to use `unsafe` rises because the project optimizes for performance, so each temptation gets a named safe alternative:

| Temptation | Safe alternative |
|---|---|
| `from_utf8_unchecked` after validating the body | `simdutf8::basic::from_utf8` already returns `&str` safely |
| `unsafe impl Send` for a tendril DOM | `with_html` on the pool, or a `Send` parser (§4.3) |
| A DOM borrowing the body `Bytes` (self-referential) | `self_cell` / `yoke`, or a parser that borrows `&str` |
| SIMD JSON | `sonic-rs` / `simd-json` behind a feature: the `unsafe` stays inside the dependency, fuzzed upstream |
| Custom arenas or zero-copy casts | the safe `bumpalo` API, or `serde` |
| `pre_exec`, `libc` | `process_group`, `sysinfo`, reading cgroup files |

**Exception process.** A profiled hotspot may justify `unsafe` only when:
- it shows a measured macro-benchmark gain;
- it is isolated in a single `crawlee-kernels` crate;
- that crate is covered by `miri`, `cargo-fuzz`, and a differential test against the safe version.

---

## 6. Extension points to keep open

### 6.1 Browser crawlers
- Reserve a `HasPage` trait and a `BrowserBackend` trait sketch. Make sure `HasHtml` can be implemented from page content as well as from an HTTP body.
- `Session` gets a fingerprint struct with browser fields, and cookies convertible to CDP format.
- Keep lifecycle cleanups in the pipeline generic, so a page can be closed through `Cleanups`.
- Reserve Cargo feature names (`browser-cdp`).

### 6.2 Adaptive crawler
- `HasHtml` / `HasResponse` must not be tied to `HttpContext`.
- `TransactionView` is needed for comparing results (§4.5).
- `RecoverableState<T>` is generic, so a rendering predictor can use it.
- User state is `Clone + Serialize`.

### 6.3 Apify SDK crate, later, over the Rust `apify-client`
**Crate direction:** `apify-sdk` depends on `crawlee-core` traits plus `apify-client`, and `crawlee` must never depend on Apify.

**Make the storage traits fit an API-backed implementation:**
- Async throughout, with pagination (`offset`/`limit`, `exclusive_start_key`).
- `push_data` takes pre-serialized bytes, so they are sent without re-serializing.
- `RequestQueueBackend::fetch_next_request` stays abstract, so the SDK can build head caching and locking on `list_and_lock_head` and prolong/delete-lock.
- `set_expected_request_processing_time` and `extend_request_processing_time` stay optional.
- `get_public_url` is async.
- `stats().rate_limit_errors` feeds the storage load signal.

**File-system backend hooks:** keep the protected hooks the JS `ApifyFileSystemStorageBackend` overrides, `key_value_store_adoption_candidates` and `purge_key_value_store`, as trait methods or config, so that INPUT survives a purge.

**Other seams the SDK needs:**
- the `EventManager` trait (for a websocket-backed platform implementation);
- the generic `Configuration` extension;
- `ProxyConfiguration` behind a trait;
- `Services` injection.

---

## 7. Testing that JS and Rust behave the same (answering question 2)

**How much the current tests help.**
- **e2e:** only about 25 of the 46 `test/e2e` scenarios are in scope: `cheerio-*`, request queue, session and proxy rotation, persist value, JSON5 input, and so on. They are Apify Actors built on the JS SDK, run against **live sites**, and assert coarsely (`requestsSucceeded > 0`, `datasetItems.length === 1`). They catch gross breakage, but **they cannot prove equivalence**. Keep them as smoke tests once the Apify SDK crate exists.
- **Unit tests:** roughly 50–60% of `/test/core` checks behavior, but they are vitest tests against the JS API. They can guide rewritten tests but cannot run against Rust.

**What is achievable**, by level:

| Level | Confidence |
|---|---|
| Pure functions and wire formats | **Near-exhaustive.** Enforced by differential fuzzing and golden files. |
| Crawl outcomes on deterministic fixtures | **High.** Exact comparison of datasets, request states, per-URL attempt counts, and statistics counters, for the scenarios that are covered. |
| Concurrency, ordering, autoscaling, timing | **Behavioral or statistical only.** Order is compared exactly only at `max_concurrency = 1`; otherwise results are compared as multisets, and timings within tolerances. |
| API ergonomics | **Not testable.** Covered by a parity matrix and review. |

**New tests** (in the `crawlee-conformance` repo):
1. **Differential tests of pure functions.**
   - Small "oracle" CLIs, JS calling the published `@crawlee/*` packages at a pinned version and Rust calling `crawlee-rs`, both reading JSONL on stdin and writing results.
   - Inputs come from golden corpora, `proptest`-generated cases, and a real-world corpus (for example, a Common Crawl sample).
   - Functions covered:
     - `uniqueKey` normalization;
     - URL resolution and `<base href>`;
     - enqueue strategy, `include`/`exclude` globs and regexes;
     - robots matching, sitemap parsing (XML, text, gzip, indexes);
     - charset detection and decoding, content-type parsing;
     - cookie parsing and jar behavior;
     - redirect rewriting rules;
     - KVS codec content-type inference, JSON5;
     - `html_to_text`, social handles, Open Graph, microdata.
2. **Wire-format conformance.**
   - JSON Schemas plus golden files: storage layout and `__metadata__.json`, Request JSON with `__crawlee`, and the persisted state of Statistics, SessionPool, RequestList, ThrottlingRequestManager and the sitemap loader.
   - **Cross-read round trips:** crawl with Rust, then resume and read with TS (and Python), and the reverse. Include a queue left in progress by a crash, in both `single` and `shared` modes.
3. **Differential scenarios** (the core of the answer).
   - A language-neutral, deterministic **fixture server** serving a synthetic site graph with configurable behavior:
     - redirect chains, `429` with `Retry-After`, 5xx errors, blocked codes, slow or truncated bodies;
     - gzip/brotli, unusual charsets, cookies, robots.txt and sitemaps;
     - malformed and very large HTML, paginated JSON APIs.
   - Each scenario is YAML: the site config, a crawler config in a neutral key set (mapped to the options in each language), and **one of about 15 canonical handlers implemented in both languages**. Examples: extract and enqueue; paginate JSON; throw on label X; session error on page Y; `use_state` counter.
   - Both implementations run it, and a comparator checks normalized output:
     - dataset items as a multiset, ignoring key order;
     - KVS entries;
     - the final request states (handled flag, `retryCount`, normalized `errorMessages`);
     - statistics counters, including the status-code histogram;
     - the **server-side traffic log** of method, URL, selected headers and cookies, with per-URL attempt counts.
4. **Crash and resume.** Kill the process at random points and resume. Check at-least-once delivery with no lost items, and duplicates only within what transactions allow. Same checks for both implementations.
5. **Parser divergence report.**
   - Run a set of common selectors (`title`, `a[href]`, `meta`, `h1`, table cells, the JSON-LD `script`) over the real-world corpus, comparing cheerio/htmlparser2 with Rust, and also parse5 with Rust.
   - Report the divergence rate. Differences against htmlparser2 on malformed HTML are an expected explicit difference; differences against parse5 are bugs.
   - JSON goes through the same process: parse and serialize differentially across the edge cases in §5.
6. **An explicit registry of allowed differences.** `allowed-differences.yaml` gives each difference an id, a rationale, a scope, and the tests that tolerate it. Anything not listed fails CI. This turns "small explicit differences" into something reviewable.
7. **A public-API parity matrix.** A script lists the public symbols in `docs/public-api/*.api.md`. Each must be mapped to a Rust item or marked N/A with a reason.
8. **Benchmarks**, which are not about equivalence but are the reason the project exists. The same fixture scenarios at 10k, 100k and 1M URLs with injected latency, measuring throughput per core, RSS, and p99. Tracked in CI.

---

## 8. Roadmap (HTTP scope)

The effort figures are rough, for 2–3 engineers who are strong in Rust.

- **Phase 0 (about 1.5–2 months): de-risk.**
  - Profile TS HTTP crawls to confirm the hotspots in §1.
  - Run the parser spike (§4.3) and the minimal Rust pipeline benchmark. Then the **go/no-go gate**.
  - Build the skeleton of the conformance repo: fixture server, oracle CLIs, golden files.
- **Phase 1 (about 2–3 months):** `crawlee-core`, `crawlee-utils`, and the storage backends, with the pure-function differential tests and wire-format tests green.
- **Phase 2 (about 2–3 months):** `crawlee-basic`: pipeline, router, sessions, concurrency and autoscaling, statistics, transactions, the throttling manager, the sitemap loader. Differential scenarios green. **Alpha.**
- **Phase 3 (about 2 months):** HTTP clients, `HttpCrawler`, `JsonCrawler`, `HtmlCrawler` with streaming link extraction, and performance tuning. **Beta.**
- **Phase 4 (about 1–2 months):** crash/resume tests, the parser-divergence report, the parity matrix, CLI and templates, the `otel` feature, docs. **1.0.**
- **Later, not in this plan:** the Apify SDK crate (§6.3), browser crawlers (§6.1), the adaptive crawler (§6.2).

**Total to 1.0: about 9–12 months.** After that, about 1 FTE of ongoing parity work.

---

## 9. Long-term maintenance: benefits and costs

**Benefits**
- **Throughput per core and memory on CPU-heavy HTTP, JSON and HTML crawls:**
  - parallel user handlers and parsing;
  - typed JSON with no intermediate tree;
  - a single serialization per item;
  - streaming link extraction;
  - a compact dedup cache;
  - small static binaries and slim images.
- **Correctness:** the invariants that the TS code enforces with `as unknown as` casts are checked by the compiler, and timeouts actually cancel work.
- **Shared correctness assets:** the conformance repo and its differential harness find parity bugs between TS and Python as well.
- **Ecosystem gap:** Rust has few batteries-included crawling frameworks.

**Costs**
- **A third implementation.** Every feature, fix, doc page and template happens three times, and parity drifts. Mitigations: the conformance CI, the explicit differences registry, and the parity matrix.
- **Review capacity.** The team is strongest in TS and Python, so Rust review becomes a bottleneck. Compile times and generic-heavy APIs slow iteration.
- **A smaller user audience than JS or Python.** The Apify SDK crate becomes its own maintenance line.
- **Why not speed up TS with native hot paths instead?** It helps less than it seems. The user's extraction code stays JS, so every selector call would cross the napi boundary, and bodies and DOMs would be copied between the two sides. The per-platform prebuild matrix is also costly, as the `--omit=optional` breakage with impit and fs-storage-native already shows. A Rust crate wins because the user's code is Rust too.

---

## 10. Reference TS files (the behavior to reproduce)

- `packages/basic-crawler/src/internals/crawlers/context_pipeline.ts`
- `packages/basic-crawler/src/internals/basic-crawler.ts`: pipeline order ~L1428, retries and errors ~L2632–2955
- `packages/basic-crawler/src/internals/{router.ts, session_pool/*, autoscaling/*, throttling_request_manager.ts, sitemap_request_loader.ts}`
- `packages/core/src/{service_locator.ts, configuration.ts, request.ts, recoverable_state.ts}`, `packages/core/src/storages/{transaction.ts, request_queue.ts, key_value_store_codec.ts}`
- `packages/types/src/{storages.ts, http-client.ts, session.ts}`: the trait contracts
- `packages/http-client/src/base-http-client.ts`, `packages/impit-client/src/index.ts`
- `packages/http-crawler/src/internals/{http-crawler.ts, dom-crawler.ts, utils.ts}`, `packages/cheerio-crawler/src/internals/cheerio-parser.ts`
- `packages/fs-storage/src/file-system-storage.ts`: the adoption and purge hooks
- `docs/public-api/*.api.md` (parity matrix), `docs/upgrading/upgrading_v4.md`, `test/e2e/*` (smoke tests to port later)

## 11. Verification (definition of done for 1.0)

- The Phase 0 benchmark gate passed, and the §7.8 benchmarks meet the targets in CI.
- All conformance levels in §7 are green in CI against pinned TS (and Python) releases. Every tolerated difference is listed in `allowed-differences.yaml`.
- Cross-language storage round trips and crash/resume runs are green.
- `cargo clippy -D warnings`, `cargo fmt --check`, `cargo deny`, the `forbid(unsafe_code)` check, `cargo semver-checks`, and doc-tested examples all pass.
