// The bench_crawl workload with Crawlee for JS (CheerioCrawler), run from its sources.
//
//   CRAWLEE_JS_DIR=/path/to/crawlee node --experimental-transform-types --no-warnings \
//       conformance/bench/cheerio-crawl.mts http://127.0.0.1:3000 10000 50
//
// Autoscaling is pinned (min = max concurrency) and storage is in memory, to match the Rust run.

import { register } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = process.env.CRAWLEE_JS_DIR;
if (!root) throw new Error('Set CRAWLEE_JS_DIR to a checkout of apify/crawlee with dependencies installed.');
register(pathToFileURL(join(dirname(fileURLToPath(import.meta.url)), '..', 'oracle', 'ts-resolve.mjs')).href);

const { CheerioCrawler } = await import(pathToFileURL(join(root, 'packages', 'cheerio-crawler', 'src', 'index.ts')).href);
const { MemoryStorageBackend } = await import(pathToFileURL(join(root, 'packages', 'core', 'src', 'index.ts')).href);

const [base = 'http://127.0.0.1:3000', pagesArg = '10000', concurrencyArg = '50'] = process.argv.slice(2);
const pages = Number(pagesArg);
const concurrency = Number(concurrencyArg);

const crawler = new CheerioCrawler({
    storageBackend: new MemoryStorageBackend(),
    minConcurrency: concurrency,
    maxConcurrency: concurrency,
    maxRequestsPerCrawl: pages,
    async requestHandler({ $, request, pushData, enqueueLinks }) {
        const products = $('li.product')
            .map((_i, el) => {
                const $el = $(el);
                return {
                    id: $el.attr('data-id') ?? '',
                    name: $el.find('.name').text().trim(),
                    price: Number.parseFloat($el.find('.price').text().split(/\s+/)[0] ?? '0') || 0,
                    url: $el.find('a.product-link').attr('href') ?? null,
                };
            })
            .get();
        await pushData({ url: request.loadedUrl ?? request.url, title: $('title').text().trim(), products });
        await enqueueLinks();
    },
});

const started = performance.now();
const stats = await crawler.run([`${base}/p/0`]);
const seconds = (performance.now() - started) / 1000;
const cpu = process.cpuUsage();
const cpuSeconds = (cpu.user + cpu.system) / 1e6;

console.log(
    JSON.stringify({
        implementation: 'crawlee-js',
        pages: stats.requestsSucceeded,
        failed: stats.requestsFailed,
        concurrency,
        seconds: Math.round(seconds * 100) / 100,
        pages_per_second: Math.round(stats.requestsSucceeded / seconds),
        cpu_seconds: Math.round(cpuSeconds * 100) / 100,
        cpu_ms_per_page: Math.round((cpuSeconds * 1000 * 100) / Math.max(1, stats.requestsSucceeded)) / 100,
        peak_rss_mib: Math.round(process.resourceUsage().maxRSS / 1024),
    }),
);
process.exit(0);
