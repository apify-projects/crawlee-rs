// Generates the golden files in ../golden from Crawlee for JS, which serves as the oracle.
//
// Usage (Node 22+):
//   CRAWLEE_JS_DIR=/path/to/crawlee node --experimental-transform-types --no-warnings \
//       conformance/oracle/generate-golden.mts
//
// CRAWLEE_JS_DIR is a checkout of apify/crawlee with dependencies installed (`pnpm install`).
// The Rust tests replay every case; a divergence either is a bug or must be listed in
// conformance/allowed-differences.md with the id of the case.

import { writeFileSync } from 'node:fs';
import { createRequire, register } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = process.env.CRAWLEE_JS_DIR;
if (!root) throw new Error('Set CRAWLEE_JS_DIR to a checkout of apify/crawlee with dependencies installed.');

register(pathToFileURL(join(dirname(fileURLToPath(import.meta.url)), 'ts-resolve.mjs')).href);

const utilsDir = join(root, 'packages', 'utils');
const requireFromUtils = createRequire(join(utilsDir, 'package.json'));
const importDep = async (name: string) => import(pathToFileURL(requireFromUtils.resolve(name)).href);

const { normalizeUrl } = await importDep('@apify/utilities');
const { getDomain } = await importDep('tldts');
const { parseDocument } = await importDep('htmlparser2');
const cheerio = await import(pathToFileURL(join(root, 'node_modules', 'cheerio', 'dist', 'esm', 'slim.js')).href).catch(
    async () => importDep('cheerio/slim'),
);
const urlUtils = await import(pathToFileURL(join(utilsDir, 'src', 'internals', 'url.ts')).href);
const cheerioUtils = await import(pathToFileURL(join(utilsDir, 'src', 'internals', 'cheerio.ts')).href);
const { Request } = await import(pathToFileURL(join(root, 'packages', 'core', 'src', 'request.ts')).href);
const { uniqueKeyToRequestId } = await import(
    pathToFileURL(join(root, 'packages', 'core', 'src', 'memory-storage', 'utils.ts')).href
);
const { RobotsTxtFile } = await import(pathToFileURL(join(utilsDir, 'src', 'internals', 'robots.ts')).href);
const { parseSitemap } = await import(pathToFileURL(join(utilsDir, 'src', 'internals', 'sitemap.ts')).href);
const { parseRetryAfterHeader } = await import(
    pathToFileURL(join(root, 'packages', 'basic-crawler', 'src', 'internals', 'http.ts')).href
);
const { extractCharsetFromHtmlBytes } = await import(
    pathToFileURL(join(root, 'packages', 'http-crawler', 'src', 'internals', 'utils.ts')).href
);

const goldenDir = join(dirname(fileURLToPath(import.meta.url)), '..', 'golden');
const write = (name: string, cases: unknown[]) => {
    writeFileSync(join(goldenDir, `${name}.json`), `${JSON.stringify(cases, null, 2)}\n`);
    console.log(`${name}.json: ${cases.length} cases`);
};

// --- URL normalization / unique keys ----------------------------------------------------------

const urls = [
    'https://example.com',
    'https://example.com/',
    'HTTP://www.EXAMPLE.com/something/',
    'https://example.com/a/b/',
    'https://example.com/a//',
    'https://example.com/?b=2&a=1',
    'https://example.com/?b=2&a=1&a=0',
    'https://example.com/?utm_source=x&utm_medium=y&id=1',
    'https://example.com/?UTM_source=x',
    'https://example.com/a?q=hello%20world',
    'https://example.com/a?q=hello+world',
    'https://example.com/a?q=a%2Bb',
    'https://example.com/a?flag',
    'https://example.com/a?',
    'https://example.com/a?=value',
    'https://example.com/a?q=%E2%9C%93',
    'https://example.com/a?q=✓&z=ž',
    'https://example.com/a?b=1&B=2&a=3',
    'https://example.com/a?%F0%9F%98%80=1&%EF%BD%81=2',
    'https://example.com/a#frag',
    'https://example.com/a#',
    'https://example.com/a?x=1#frag',
    'https://example.com:443/a',
    'http://example.com:80/a',
    'https://example.com:8443/a',
    'https://user:pass@example.com/a',
    'https://EXAMPLE.com/A/B',
    'https://example.com/a%2fb',
    'https://example.com/a b',
    'https://example.com/ümlaut',
    'https://xn--mnchen-3ya.de/',
    'https://münchen.de/',
    'https://example.com/a/../b/./c',
    'https://[::1]:8080/a/',
    'http://127.0.0.1:3000/x/',
    '  https://example.com/trim  ',
    'https://example.com/a?q=1;2&x=[1]',
    'https://example.com/a?q=%zz',
    'not a url',
    '',
    'ftp://files.example.com/pub/',
    'mailto:someone@example.com',
];

write(
    'normalize_url',
    urls.flatMap((input) =>
        [false, true].map((keepFragment) => ({ input, keepFragment, expected: normalizeUrl(input, keepFragment) })),
    ),
);

const keyCases = [
    { url: 'https://example.com/a/' },
    { url: 'https://example.com/a?b=1&a=2#x' },
    { url: 'https://example.com/a#x', keepUrlFragment: true },
    { url: 'https://example.com/a', method: 'post', payload: '{"q":1}', useExtendedUniqueKey: true },
    { url: 'https://example.com/a', method: 'PUT', payload: 'x=1&y=2', useExtendedUniqueKey: true },
    { url: 'https://example.com/a', method: 'POST', useExtendedUniqueKey: true },
    { url: 'https://example.com/a', method: 'POST', payload: 'ignored without extended key' },
    { url: 'not a url' },
];
write(
    'unique_key',
    keyCases.map((options) => {
        const request = new Request(options);
        return { options, uniqueKey: request.uniqueKey, requestId: uniqueKeyToRequestId(request.uniqueKey) };
    }),
);

// --- Request JSON -------------------------------------------------------------------------------

const requestCases = [
    { url: 'https://example.com/a' },
    { url: 'https://example.com/b', label: 'DETAIL', userData: { page: 2, tags: ['x'] } },
    { url: 'https://example.com/c', crawlDepth: 3, maxRetries: 5, skipNavigation: true, sessionId: 'session_1' },
    { url: 'https://example.com/d', method: 'POST', payload: 'q=1', headers: { 'X-Test': '1' }, noRetry: true },
    { url: 'https://example.com/e', uniqueKey: 'custom-key', enqueueStrategy: 'same-domain' },
];
write(
    'request_json',
    requestCases.map((options) => ({ options, json: JSON.parse(JSON.stringify(new Request(options))) })),
);

// --- Registrable domains and enqueue strategies -------------------------------------------------

const hostnames = [
    'example.com',
    'www.example.com',
    'a.b.c.example.com',
    'example.co.uk',
    'shop.example.co.uk',
    'co.uk',
    'com',
    'foo.github.io',
    'github.io',
    'foo.blogspot.com',
    'something.appspot.com',
    'example.com.',
    'localhost',
    'my.localhost',
    'intranet',
    'server.local',
    'foo.bar.unknowntld',
    '127.0.0.1',
    '[::1]',
    'xn--mnchen-3ya.de',
    'www.xn--mnchen-3ya.de',
    'foo.city.kawasaki.jp',
    'www.ck',
    'foo.www.ck',
    'sub.example.pvt.k12.ma.us',
];
write(
    'registrable_domain',
    hostnames.map((hostname) => ({ hostname, expected: getDomain(hostname, { mixedInputs: false }) })),
);

const origins = ['https://www.example.com/start', 'http://127.0.0.1:8080/', 'https://foo.github.io/'];
const targets = [
    'https://www.example.com/x',
    'http://www.example.com/x',
    'https://example.com/x',
    'https://shop.example.com/x',
    'https://www.example.com:8443/x',
    'https://www.example.com./x',
    'https://example.org/x',
    'http://127.0.0.1:8080/x',
    'http://127.0.0.1:9090/x',
    'https://bar.github.io/x',
    'https://foo.github.io/x',
];
const strategies = ['all', 'same-hostname', 'same-domain', 'same-origin'];
write(
    'enqueue_strategy',
    origins.flatMap((origin) =>
        targets.flatMap((target) =>
            strategies.map((strategy) => ({
                strategy,
                origin,
                target,
                expected: urlUtils.matchesEnqueueStrategy(strategy, new URL(target), new URL(origin)),
            })),
        ),
    ),
);

// --- Link extraction (as CheerioCrawler parses pages) --------------------------------------------

const linkCases: { id: string; html: string; selector?: string; baseUrl: string }[] = [
    { id: 'basic', html: '<a href="/a">A</a><a href="b">B</a><a href="https://other.dev/c">C</a>', baseUrl: 'https://example.com/dir/page' },
    { id: 'entities', html: '<a href="?a=1&amp;b=2">x</a><a href="&#47;slash">y</a><a href="/q?x=1&copy=2">z</a><a href="/e?a&lt;b">w</a>', baseUrl: 'https://example.com/' },
    { id: 'empty-and-missing', html: '<a href="">e</a><a>none</a><a href="   ">spaces</a><a href="#top">frag</a>', baseUrl: 'https://example.com/p' },
    { id: 'base-href', html: '<head><base href="/root/"></head><a href="x">X</a><a href="/abs">abs</a>', baseUrl: 'https://example.com/dir/page' },
    { id: 'base-after-links', html: '<a href="x">X</a><base href="https://cdn.example.com/b/">', baseUrl: 'https://example.com/dir/page' },
    { id: 'two-bases', html: '<base href="/first/"><base href="/second/"><a href="x">X</a>', baseUrl: 'https://example.com/' },
    { id: 'schemes', html: '<a href="mailto:a@b.c">m</a><a href="javascript:void(0)">js</a><a href="tel:+420">t</a><a href="//proto-relative.dev/x">p</a>', baseUrl: 'https://example.com/' },
    { id: 'whitespace', html: '<a href="  /padded  ">p</a><a href="/tab\tinside">t</a><a href="/new\nline">n</a>', baseUrl: 'https://example.com/' },
    { id: 'unicode', html: '<a href="/ümlaut">u</a><a href="https://münchen.de/x">idn</a><a href="/a b">space</a>', baseUrl: 'https://example.com/' },
    { id: 'uppercase-tags', html: '<A HREF="/upper">U</A><a Href="/mixed">M</a>', baseUrl: 'https://example.com/' },
    { id: 'selector-class', html: '<a class="next" href="/2">n</a><a href="/other">o</a><div class="next"><a href="/in-div">d</a></div>', selector: 'a.next', baseUrl: 'https://example.com/' },
    { id: 'selector-descendant', html: '<nav><a href="/nav">n</a></nav><main><a href="/main">m</a></main>', selector: 'nav a', baseUrl: 'https://example.com/' },
    { id: 'selector-attribute', html: '<a href="/a" rel="next">a</a><a href="/b">b</a><link rel="next" href="/link">', selector: '[rel=next]', baseUrl: 'https://example.com/' },
    { id: 'unclosed-quotes', html: '<a href="/ok">ok</a><a href="/broken>broken</a><a href="/after">after</a>', baseUrl: 'https://example.com/' },
    { id: 'nested-anchors', html: '<a href="/outer"><a href="/inner">i</a></a>', baseUrl: 'https://example.com/' },
    { id: 'links-in-script-and-comments', html: '<script>var s = "<a href=\'/in-script\'>x</a>";</script><!-- <a href="/in-comment">c</a> --><a href="/real">r</a>', baseUrl: 'https://example.com/' },
    { id: 'template-and-noscript', html: '<template><a href="/in-template">t</a></template><noscript><a href="/in-noscript">n</a></noscript>', baseUrl: 'https://example.com/' },
    { id: 'svg-links', html: '<svg><a href="/svg-href">s</a><a xlink:href="/svg-xlink">x</a></svg>', baseUrl: 'https://example.com/' },
];
write(
    'extract_links',
    linkCases.map(({ id, html, selector = 'a', baseUrl }) => {
        const $ = cheerio.load(parseDocument(html, { decodeEntities: true }));
        return { id, html, selector, baseUrl, expected: cheerioUtils.extractUrlsFromCheerio($, selector, baseUrl) };
    }),
);

// --- Charset prescan ----------------------------------------------------------------------------

const charsetCases = [
    '<meta charset="utf-8">',
    '<meta charset=windows-1250>',
    "<meta charset='ISO-8859-2'>",
    '<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=Shift_JIS">',
    '<meta name="x" content="y"><meta charset="koi8-r">',
    '<html><head><title>no charset</title>',
    `${' '.repeat(1020)}<meta charset="late">`,
];
write(
    'charset_prescan',
    charsetCases.map((html) => ({ html, expected: extractCharsetFromHtmlBytes(Buffer.from(html, 'latin1')) ?? null })),
);

// --- JSON as stored in key-value stores (JSON.stringify(value, null, 2)) ------------------------

const jsonCases = [
    { a: 1, b: 'text', c: [1, 2, { d: null }], e: {}, f: [] },
    { unicode: 'Žluťoučký kůň ✓ 😀', escapes: 'quote " backslash \\ newline \n tab \t slash /', control: '\u0001' },
    { integer: 42, negative: -7, float: 0.1, exp: 1e21, small: 1e-7, big: 12345678901234567890, float_int: 1.0 },
    { nested: { deeper: { deepest: [true, false] } } },
    ['top', 'level', 'array'],
];
write(
    'kvs_json',
    jsonCases.map((value) => ({ value, text: JSON.stringify(value, null, 2) })),
);

// --- robots.txt (robots-parser, through RobotsTxtFile) -----------------------------------------

const robotsFiles = [
    {
        id: 'groups',
        content: [
            'User-agent: *',
            'Disallow: /private/',
            'Allow: /private/public',
            'Disallow: /*.pdf$',
            'Disallow: /search?',
            'Crawl-delay: 2',
            '',
            'User-agent: GoodBot',
            'User-agent: other-bot/2.0',
            'Disallow:',
            'Crawl-delay: 0.5',
            '',
            'User-agent: BadBot',
            'Disallow: /',
            '',
            'Sitemap: https://example.com/sitemap.xml',
            'Sitemap: https://other.com/sitemap.xml',
            'Sitemap: https://sub.example.com/s.xml',
        ].join('\n'),
    },
    {
        id: 'ties-and-encoding',
        content: 'user-agent: *\r\ndisallow: /page\r\nallow: /page\r\nDisallow: /caf\u00e9\rDisallow: /a%2fb # comment\nDisallow: /x*y$\nAllow: /*.html$',
    },
    { id: 'no-rules', content: '# nothing here\nSitemap:\nfoo bar\nUser-agent: *\nCrawl-delay: soon' },
    { id: 'wildcard-only', content: 'User-agent: *\nDisallow: *\nAllow: /$' },
];
const robotsUrls = [
    'https://example.com/',
    'https://example.com/private/x',
    'https://example.com/private/public/x',
    'https://example.com/a/b.pdf',
    'https://example.com/a/b.pdf?x=1',
    'https://example.com/search?q=1',
    'https://example.com/search',
    'https://example.com/page',
    'https://example.com/caf%C3%A9',
    'https://example.com/café',
    'https://example.com/a%2Fb',
    'https://example.com/a%2fb',
    'https://example.com/xzzy',
    'https://example.com/xzzy/',
    'https://example.com/index.html',
    'http://example.com/private/x',
    'https://example.com:8443/private/x',
    'https://other.com/private/x',
];
const userAgents = ['*', 'GoodBot', 'goodbot/1.0', 'other-bot', 'BadBot', 'Unknown'];
write(
    'robots_txt',
    robotsFiles.map(({ id, content }) => {
        const robots = RobotsTxtFile.from('https://example.com/robots.txt', content);
        return {
            id,
            content,
            sitemaps: robots.getSitemaps(),
            crawlDelay: Object.fromEntries(userAgents.map((ua) => [ua, robots.getCrawlDelay(ua) ?? null])),
            allowed: robotsUrls.flatMap((url) => userAgents.map((ua) => ({ url, ua, allowed: robots.isAllowed(url, ua) }))),
        };
    }),
);

// --- Sitemap XML parsing ----------------------------------------------------------------------

const sitemapCases = [
    {
        id: 'urlset',
        content: `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc> https://example.com/a?x=1&amp;y=2 </loc><lastmod>2024-05-01</lastmod><changefreq>daily</changefreq><priority>0.8</priority></url>
  <url><loc><![CDATA[https://example.com/b]]></loc><changefreq>sometimes</changefreq><priority>high</priority></url>
  <url><loc>https://example.com/c</loc><lastmod>2024-05-01T10:20:30+02:00</lastmod><lastmod>not a date</lastmod></url>
  <url><loc>https://example.com/d</loc><lastmod>2024-05-01T10:20Z</lastmod><priority>1</priority></url>
  <url><lastmod>2024-01-01</lastmod></url>
  <url><loc>https://example.com/&#233;t&#xE9;</loc></url>
</urlset>`,
    },
    {
        id: 'sitemapindex',
        content: `<sitemapindex><sitemap><loc>https://example.com/s1.xml</loc><lastmod>2024-01-01</lastmod></sitemap>
<sitemap><loc>https://example.com/s2.xml.gz</loc></sitemap></sitemapindex>`,
    },
    { id: 'no-root', content: '<foo><url><loc>https://example.com/x</loc></url></foo>' },
];
const sitemapItems = async (content: string) => {
    const items = [];
    for await (const item of parseSitemap([{ type: 'raw', content }], undefined, { emitNestedSitemaps: true, maxDepth: 0 })) {
        items.push(
            item.originSitemapUrl === null
                ? { sitemap: item.loc }
                : {
                      loc: item.loc,
                      lastmod: item.lastmod?.toISOString() ?? null,
                      changefreq: item.changefreq ?? null,
                      priority: item.priority === undefined || Number.isNaN(item.priority) ? null : item.priority,
                  },
        );
    }
    return items;
};
write(
    'sitemap_xml',
    await Promise.all(sitemapCases.map(async ({ id, content }) => ({ id, content, items: await sitemapItems(content) }))),
);

// --- Retry-After ------------------------------------------------------------------------------

const retryAfterValues = ['', '0', '5', ' 120 ', '-1', '1.5', 'abc', 'Wed, 21 Oct 2015 07:28:00 GMT'];
write(
    'retry_after',
    retryAfterValues.map((value) => ({ value, millis: parseRetryAfterHeader(value) })),
);
