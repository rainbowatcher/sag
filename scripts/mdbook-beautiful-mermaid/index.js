#!/usr/bin/env node
import { renderMermaidSVG } from 'beautiful-mermaid';

const args = process.argv.slice(2);

// mdbook preprocessor protocol: supports <renderer>
if (args[0] === 'supports') {
  const renderer = args[1];
  console.log(renderer === 'html' ? 'true' : 'false');
  process.exit(0);
}

// Statistics reported to stderr after a successful run.
const stats = { rendered: 0, cached: 0, errors: 0 };

// Cache so identical mermaid sources are only rendered once per process.
// mdbook may invoke the preprocessor with the same source on retry / hot reload.
const renderCache = new Map();

/**
 * Replace the inline color values in the <svg> root's `style` attribute with
 * CSS variable references, so theme switching works via `docs/theme/mermaid.css`.
 *
 * We deliberately do NOT use a generic `/style="[^"]*"/` regex, which would
 * match the *first* style attribute anywhere in the document. Instead we
 * locate the opening <svg ...> tag and only mutate the style= that appears
 * inside it. This is robust against future changes in beautiful-mermaid's
 * output ordering.
 */
function injectMermaidThemeVars(svg) {
  const styleVars =
    '--bg:var(--m-bg,#eceff4);' +
    '--fg:var(--m-fg,#2e3440);' +
    '--accent:var(--m-accent,#5e81ac);' +
    '--surface:var(--m-surface,#d8dee9);' +
    '--border:var(--m-border,#81a1c1);' +
    '--line:var(--m-line,#81a1c1);' +
    '--muted:var(--m-muted,#60728a)';

  const openStart = svg.indexOf('<svg');
  if (openStart < 0) return svg; // defensive: no <svg> root
  const openEnd = svg.indexOf('>', openStart);
  if (openEnd < 0) return svg;
  const openTag = svg.slice(openStart, openEnd + 1);

  const styleRe = /\sstyle="[^"]*"/;
  const newOpenTag = styleRe.test(openTag)
    ? openTag.replace(styleRe, ` class="mermaid-svg" style="${styleVars}"`)
    : openTag.replace(/^<svg/, `<svg class="mermaid-svg" style="${styleVars}"`);

  return newOpenTag + svg.slice(openEnd + 1);
}

function wrapError(message, source, where) {
  // Use <pre> for both the message and the source so a developer can copy
  // either into a REPL to reproduce.
  const safeMsg = escapeHtml(message);
  const safeSrc = escapeHtml(source);
  return (
    `<div class="mermaid-error" data-source-path="${escapeHtml(where)}">` +
    `<pre class="mermaid-error-msg">${safeMsg}</pre>` +
    `<pre class="mermaid-error-src">${safeSrc}</pre>` +
    `</div>`
  );
}

function escapeHtml(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function logWarn(msg) {
  process.stderr.write(`[beautiful-mermaid] warn: ${msg}\n`);
}

function logError(msg) {
  process.stderr.write(`[beautiful-mermaid] error: ${msg}\n`);
}


/**
 * Process the mdbook Book object in place, transforming every mermaid code
 * block inside every chapter (recursively, including nested sub_items).
 */
function processBook(book, pathStack = []) {
  if (!book || typeof book !== 'object') return;
  const items = book.sections ?? book.items ?? [];
  if (!Array.isArray(items)) {
    logWarn(`Expected sections/items to be an array, got ${typeof items}. Skipping.`);
    return;
  }

  for (const item of items) {
    if (!item || !item.Chapter) continue;
    const chapter = item.Chapter;
    const name = chapter.name || '<unnamed>';
    const nextPath = [...pathStack, name];

    if (typeof chapter.content === 'string' && chapter.content.length > 0) {
      chapter.content = renderMermaidBlocks(chapter.content, nextPath);
    }

    if (chapter.sub_items !== undefined) {
      if (!Array.isArray(chapter.sub_items)) {
        logWarn(`Chapter "${nextPath.join(' / ')}": sub_items is not an array, skipping.`);
      } else {
        // sub_items elements are BookItem-shaped; pass them as a synthetic
        // book's `sections` so the same loop can process them recursively.
        processBook({ sections: chapter.sub_items }, nextPath);
      }
    }
  }
}

function renderMermaidBlocks(content, chapterPath) {
  const mermaidRegex = /```mermaid\r?\n([\s\S]*?)```/g;
  return content.replace(mermaidRegex, (match, code) => {
    const source = code.replace(/\r\n?/g, '\n').trim();
    const where = chapterPath.join(' / ') || '<root>';

    // Cache hit: reuse the previously rendered output verbatim.
    if (renderCache.has(source)) {
      stats.cached += 1;
      return renderCache.get(source);
    }

    let svg;
    try {
      svg = renderMermaidSVG(source, {
        bg: '#eceff4',
        fg: '#2e3440',
        accent: '#5e81ac',
        surface: '#d8dee9',
        border: '#81a1c1',
        line: '#81a1c1',
        muted: '#60728a',
        transparent: true,
        font: 'Inter, system-ui, sans-serif',
      });
    } catch (err) {
      stats.errors += 1;
      logError(`mermaid render failed in "${where}": ${err.message}`);
      const wrapped = wrapError(err.message, source, where);
      // Do NOT cache failures: transient parser issues should be retried.
      return wrapped;
    }

    const themed = injectMermaidThemeVars(svg);
    const wrapped = `<div class="mermaid">${themed}</div>`;
    renderCache.set(source, wrapped);
    stats.rendered += 1;
    return wrapped;
  });
}

// ── main ──────────────────────────────────────────────────────────────
// Read the full stdin (mdbook sends a single JSON blob), then process it.
let input = '';
process.stdin.setEncoding('utf-8');
process.stdin.on('data', (chunk) => (input += chunk));
process.stdin.on('end', () => {
  let parsed;
  try {
    parsed = JSON.parse(input);
  } catch (err) {
    logError(`failed to parse stdin JSON: ${err.message}`);
    process.exit(1);
  }

  if (!Array.isArray(parsed) || parsed.length < 2) {
    logError('stdin JSON must be a [context, book] tuple per the mdbook preprocessor protocol.');
    process.exit(1);
  }
  const [, book] = parsed;

  try {
    processBook(book);
  } catch (err) {
    logError(`unhandled error while processing book: ${err.stack || err.message}`);
    process.exit(1);
  }

  // Emit stats to stderr so they don't pollute the JSON mdbook reads from stdout.
  if (stats.rendered + stats.cached + stats.errors > 0) {
    process.stderr.write(
      `[beautiful-mermaid] rendered=${stats.rendered} cached=${stats.cached} errors=${stats.errors}\n`,
    );
  }

  process.stdout.write(JSON.stringify(book));
});
