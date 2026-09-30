# Conformance with Crawlee for JS

The TypeScript implementation is the reference for crawlee-rs's behavior. This directory holds
the tooling that keeps the two in sync.

| Path | What it is |
|---|---|
| `oracle/generate-golden.mts` | Runs Crawlee for JS **from its sources** (no build) and records its outputs for a list of inputs |
| `oracle/ts-resolve.mjs` | Node resolve hook that makes this possible: `.js` → `.ts` specifiers, `@crawlee/*` → `packages/*/src` |
| `golden/*.json` | The recorded cases, committed. `crates/crawlee/tests/golden.rs` replays them against the Rust code |
| `allowed-differences.md` | Every deliberate difference, with the reason |
| `bench/cheerio-crawl.mts` | The benchmark workload for Crawlee for JS, the counterpart of `crates/crawlee/examples/bench_crawl.rs` |

## Regenerating the golden files

Do this after changing the cases, or to check a new Crawlee for JS version:

```sh
git clone https://github.com/apify/crawlee ../crawlee && (cd ../crawlee && corepack enable && pnpm install)
CRAWLEE_JS_DIR=../crawlee node --experimental-transform-types --no-warnings conformance/oracle/generate-golden.mts
cargo test -p crawlee --test golden
```

Node 22 or later is required, for type stripping.

## Running the benchmark

```sh
cargo build --release --examples
./target/release/examples/bench_server 127.0.0.1:3000 20000 &        # [latency_ms] as 3rd argument
./target/release/examples/bench_crawl http://127.0.0.1:3000 5000 50   # pages, concurrency
CRAWLEE_JS_DIR=../crawlee CRAWLEE_LOG_LEVEL=WARNING \
  node --experimental-transform-types --no-warnings conformance/bench/cheerio-crawl.mts http://127.0.0.1:3000 5000 50
```

- Run the server as its own process, so its CPU time is not counted against either crawler.
- Both crawl clients print one JSON line with pages per second, CPU time per page and peak RSS.

## What comes next

These follow the plan in `docs/plan.md`:

1. **Property-based differential tests** of the pure functions, with generated URLs and HTML sent
   through both implementations.
2. **A real-world corpus.** Selector and link-extraction results on a sample of real pages, which
   measures the html5ever-vs-htmlparser2 divergence rate.
3. **Scenario runs.**
   - The same crawl, with about 15 canonical handlers written in both languages, run by both
     implementations against the fixture server.
   - The comparison covers the dataset contents, final request states, per-URL attempt counts
     from the server log, and statistics counters.
4. **Storage round trips.** Crawl with one implementation, then resume and read with the other.
   This needs the file-system backend first.
