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

export async function resolve(specifier, context, nextResolve) {
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
