// Node ESM loader that teaches `node --experimental-strip-types` the `@/`
// path alias from `tsconfig.json` (`@/*` → `./src/*`).
//
// The node-native test runner (`node --test`) has no bundler, so a module that
// value-imports through the alias — every Pinia store does, e.g.
// `import { api } from "@/lib/client"` — fails with `Cannot find package
// '@/lib'`. Vite resolves the alias for the app; this hook does the same for
// the tests, which lets store modules be imported directly.
//
// Node's ESM resolver also does no extension probing, so a specifier without
// an extension — aliased (`@/lib/client`) or relative (`./connection`) — is
// completed with `.ts` / `.tsx` / `/index.ts`. Extension probing is limited to
// modules inside `src/` so `node_modules` resolution stays untouched.
//
// Registered from the `test` script via `--import ./scripts/alias-loader.mjs`;
// no runtime dependency, no change to any existing test file.

import { register } from "node:module"

const SRC_URL = new URL("../src/", import.meta.url).href

// The hooks run on the loader thread, so they are shipped as a `data:` module
// instead of a second file on disk.
const hooksSource = `
import { existsSync } from "node:fs"
import { fileURLToPath } from "node:url"

const SRC_URL = ${JSON.stringify(SRC_URL)}

function withExtension(url) {
  const candidates = [url, url + ".ts", url + ".tsx", url + "/index.ts"]
  for (const candidate of candidates) {
    if (existsSync(fileURLToPath(candidate))) return candidate
  }
  return url
}

export async function resolve(specifier, context, nextResolve) {
  if (specifier.startsWith("@/")) {
    return nextResolve(withExtension(new URL(specifier.slice(2), SRC_URL).href), context)
  }
  const fromSrc = context.parentURL?.startsWith(SRC_URL) === true
  if (fromSrc && (specifier.startsWith("./") || specifier.startsWith("../"))) {
    return nextResolve(withExtension(new URL(specifier, context.parentURL).href), context)
  }
  return nextResolve(specifier, context)
}
`

register(`data:text/javascript,${encodeURIComponent(hooksSource)}`, import.meta.url)
