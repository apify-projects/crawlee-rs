// Lets Node run Crawlee for JS straight from its TypeScript sources (no build step):
// - relative imports use `.js` specifiers meant for the compiled output, so fall back to `.ts`;
// - `@crawlee/*` workspace packages point at an unbuilt `dist/`, so map them to `src/index.ts`
//   (and `@crawlee/<pkg>/internal` to `src/internal.ts`).
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const root = process.env.CRAWLEE_JS_DIR;
const workspace = new Map();
if (root) {
    for (const dir of readdirSync(join(root, 'packages'))) {
        const manifest = join(root, 'packages', dir, 'package.json');
        if (existsSync(manifest)) {
            workspace.set(JSON.parse(readFileSync(manifest, 'utf8')).name, join(root, 'packages', dir));
        }
    }
}

function workspaceSource(specifier) {
    for (const [name, dir] of workspace) {
        if (specifier === name) return join(dir, 'src', 'index.ts');
        if (specifier.startsWith(`${name}/`)) {
            const sub = specifier.slice(name.length + 1);
            for (const candidate of [join(dir, 'src', `${sub}.ts`), join(dir, 'src', sub, 'index.ts')]) {
                if (existsSync(candidate)) return candidate;
            }
        }
    }
    return undefined;
}

// `sax` is CommonJS without detectable named exports, so `(await import('sax')).SAXParser` is
// undefined under Node's ESM loader (a bundler provides it). Serve a shim that re-exports it.
const SAX_SHIM = 'crawlee-oracle:sax-shim';
let saxUrl;

export async function load(url, context, nextLoad) {
    if (url === SAX_SHIM) {
        return {
            format: 'module',
            shortCircuit: true,
            source: `import sax from ${JSON.stringify(saxUrl)};\nexport const SAXParser = sax.SAXParser;\nexport default sax;`,
        };
    }
    return nextLoad(url, context);
}

export async function resolve(specifier, context, nextResolve) {
    if (specifier === 'sax') {
        saxUrl ??= (await nextResolve(specifier, context)).url;
        return { url: SAX_SHIM, shortCircuit: true };
    }
    const source = workspaceSource(specifier);
    if (source) {
        return nextResolve(pathToFileURL(source).href, context);
    }
    try {
        return await nextResolve(specifier, context);
    } catch (error) {
        if (error?.code === 'ERR_MODULE_NOT_FOUND' && specifier.startsWith('.') && specifier.endsWith('.js')) {
            return nextResolve(`${specifier.slice(0, -3)}.ts`, context);
        }
        throw error;
    }
}
