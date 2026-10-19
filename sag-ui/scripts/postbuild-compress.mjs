/**
 * Post-build pre-compression for the Vite output.
 *
 * Run automatically by `pnpm run build` (`vite build && node scripts/postbuild-compress.mjs`).
 * Every text asset under `sag-ui/dist/` gets a sibling `.br` and `.gz` copy so the
 * gateway can later serve them straight from the rust-embed table when the client
 * sends `Accept-Encoding` — no per-request compression on the hot path.
 *
 * Zero dependencies: only `node:zlib`, so nothing new lands in package.json.
 *
 * Rules:
 *   a. Text extensions (`.html .js .css .svg .json .webmanifest`) are compressed.
 *   b. `*.map` and already-compressed formats (`.png .jpg .jpeg .gif .webp .ico
 *      .woff .woff2 .zip .gz .br`) are skipped — both the sources and the `.br`
 *      /`.gz` files this script writes, so repeated runs stay idempotent.
 *   c. Brotli quality 11 and gzip level 9: maximum ratio, paid at build time.
 *   d. An output is only written when it is at least 2% smaller than the source
 *      (`< 98%`); incompressible files would otherwise just double the dist size.
 *
 * The script resolves `dist/` relative to its own URL, never the cwd, so it works
 * from any caller (`just build-test`, the release workflow, the Dockerfile).
 */
import { readdirSync, readFileSync, statSync, writeFileSync } from "node:fs"
import { extname, join } from "node:path"
import { fileURLToPath } from "node:url"
import { brotliCompressSync, constants as zlibConstants, gzipSync } from "node:zlib"

const distDir = fileURLToPath(new URL("../dist/", import.meta.url))

const TEXT_EXTENSIONS = new Set([".html", ".js", ".css", ".svg", ".json", ".webmanifest"])
const SKIP_EXTENSIONS = new Set([
  ".map",
  ".png",
  ".jpg",
  ".jpeg",
  ".gif",
  ".webp",
  ".ico",
  ".woff",
  ".woff2",
  ".zip",
  ".gz",
  ".br",
])

/** Ratio threshold: drop outputs that barely beat the source (0.98 = must save > 2%). */
const MIN_RATIO = 0.98

const formatBytes = (bytes) => {
  if (bytes < 1024) return `${bytes} B`
  const units = ["KiB", "MiB", "GiB"]
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(1)} ${units[unit]}`
}

const formatRatio = (compressed, original) =>
  original === 0 ? "n/a" : `${((compressed / original) * 100).toFixed(1)}%`

function walk(dir) {
  const out = []
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
    a.name.localeCompare(b.name),
  )) {
    const path = join(dir, entry.name)
    if (entry.isDirectory()) out.push(...walk(path))
    else if (entry.isFile()) out.push(path)
  }
  return out
}

function compress(source, path) {
  const br = brotliCompressSync(source, {
    params: { [zlibConstants.BROTLI_PARAM_QUALITY]: 11 },
  })
  const gz = gzipSync(source, { level: 9 })
  const written = []
  if (br.length < source.length * MIN_RATIO) {
    writeFileSync(`${path}.br`, br)
    written.push({ ext: ".br", bytes: br.length })
  }
  if (gz.length < source.length * MIN_RATIO) {
    writeFileSync(`${path}.gz`, gz)
    written.push({ ext: ".gz", bytes: gz.length })
  }
  return written
}

function main() {
  let stats
  try {
    stats = statSync(distDir)
  } catch {
    console.error(`✗ dist not found at ${distDir} — run \`vite build\` first`)
    process.exit(1)
  }
  if (!stats.isDirectory()) {
    console.error(`✗ ${distDir} is not a directory`)
    process.exit(1)
  }

  let processed = 0
  let skipped = 0
  let brCount = 0
  let gzCount = 0
  let originalBytes = 0
  let compressedBytes = 0
  let incompressible = 0

  for (const path of walk(distDir)) {
    const ext = extname(path).toLowerCase()
    if (SKIP_EXTENSIONS.has(ext) || !TEXT_EXTENSIONS.has(ext)) {
      skipped += 1
      continue
    }
    const source = readFileSync(path)
    const written = compress(source, path)
    processed += 1
    if (written.length === 0) {
      incompressible += 1
      continue
    }
    originalBytes += source.length
    for (const output of written) {
      compressedBytes += output.bytes
      if (output.ext === ".br") brCount += 1
      else gzCount += 1
    }
  }

  console.log(
    `✓ pre-compressed ${processed} text asset(s) in ${distDir} ` +
      `(${brCount} .br, ${gzCount} .gz; ${skipped} file(s) skipped)`,
  )
  console.log(
    `  ${formatBytes(originalBytes)} → ${formatBytes(compressedBytes)} ` +
      `(${formatRatio(compressedBytes, originalBytes)} of the source set)`,
  )
  if (incompressible > 0) {
    console.log(`  ℹ ${incompressible} file(s) skipped: compression saved less than 2%`)
  }
}

main()
