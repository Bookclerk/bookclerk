/**
 * Smoke checks for safeHtml sanitizer behavior (no test runner in ui/).
 * Run: node scripts/check-safe-html.mjs
 *
 * Duplicates the pure-string algorithms from src/lib/safeHtml.ts so we can
 * verify without a DOM. Keep in sync when changing sanitizer rules.
 */
import assert from "node:assert/strict";

function escapeHtml(raw) {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const ALLOWED_OPEN = {
  P: "<p>",
  B: "<b>",
  STRONG: "<strong>",
  I: "<i>",
  EM: "<em>",
  UL: "<ul>",
  OL: "<ol>",
  LI: "<li>",
};
const ALLOWED_CLOSE = {
  P: "</p>",
  B: "</b>",
  STRONG: "</strong>",
  I: "</i>",
  EM: "</em>",
  UL: "</ul>",
  OL: "</ol>",
  LI: "</li>",
};
const NAMED = {
  amp: "&",
  lt: "<",
  gt: ">",
  quot: '"',
  apos: "'",
  nbsp: "\u00A0",
  rsquo: "\u2019",
};

function decodeHtmlEntities(raw) {
  if (!raw.includes("&")) return raw;
  return raw.replace(/&(#x[0-9a-fA-F]+|#\d+|[a-zA-Z][a-zA-Z0-9]+);/g, (entity, body) => {
    if (body.startsWith("#")) {
      const code =
        body[1] === "x" || body[1] === "X"
          ? Number.parseInt(body.slice(2), 16)
          : Number.parseInt(body.slice(1), 10);
      return String.fromCodePoint(code);
    }
    return NAMED[body.toLowerCase()] ?? entity;
  });
}

function sanitizeMarkupToAllowlist(decoded) {
  const tokenRe = /<\/?([a-zA-Z][a-zA-Z0-9]*)\b[^>]*>/g;
  let out = "";
  let last = 0;
  let skipUntil = null;
  for (const match of decoded.matchAll(tokenRe)) {
    const idx = match.index ?? 0;
    const token = match[0];
    const tag = match[1].toUpperCase();
    const closing = token.startsWith("</");
    if (skipUntil !== null) {
      if (closing && tag === skipUntil) skipUntil = null;
      last = idx + token.length;
      continue;
    }
    out += escapeHtml(decoded.slice(last, idx));
    last = idx + token.length;
    if (!closing && (tag === "SCRIPT" || tag === "STYLE" || tag === "NOSCRIPT")) {
      skipUntil = tag;
      continue;
    }
    if (tag === "BR" && !closing) {
      out += "<br>";
      continue;
    }
    if (closing) {
      const close = ALLOWED_CLOSE[tag];
      if (close) out += close;
      continue;
    }
    const open = ALLOWED_OPEN[tag];
    if (open) out += open;
  }
  out += escapeHtml(decoded.slice(last));
  return out.trim();
}

function prepareDescriptionHtml(raw) {
  const decoded = decodeHtmlEntities(raw.trim());
  const trimmed = decoded.trim();
  if (!trimmed) return "";
  if (!/<[a-zA-Z!/?]/.test(trimmed)) {
    return escapeHtml(trimmed).replace(/\r\n|\r|\n/g, "<br>");
  }
  return sanitizeMarkupToAllowlist(trimmed);
}

function replaceToFixedPoint(input, pattern, replacement) {
  let previous;
  let current = input;
  do {
    previous = current;
    current = current.replace(pattern, replacement);
  } while (current !== previous);
  return current;
}

function stripTagsFallback(raw) {
  let s = decodeHtmlEntities(raw);
  s = replaceToFixedPoint(s, /<br\s*\/?>/gi, "\n");
  s = replaceToFixedPoint(s, /<\/p>/gi, "\n");
  s = replaceToFixedPoint(s, /<[^<>]*>/g, "");
  s = s.replace(/[<>]/g, "");
  return s.replace(/\s+\n/g, "\n").trim();
}

// --- assertions ---
assert.equal(decodeHtmlEntities("it&rsquo;s"), "it\u2019s");
assert.equal(decodeHtmlEntities("&#8217;"), "\u2019");
assert.equal(decodeHtmlEntities("&amp;lt;"), "&lt;");

assert.equal(prepareDescriptionHtml("hello\nworld"), "hello<br>world");
assert.equal(prepareDescriptionHtml("<p>Hi</p>"), "<p>Hi</p>");
assert.equal(
  prepareDescriptionHtml('<p class="x" onclick="alert(1)">Hi</p>'),
  "<p>Hi</p>",
);
assert.equal(prepareDescriptionHtml("<b>Bold</b> & plain"), "<b>Bold</b> &amp; plain");
assert.equal(prepareDescriptionHtml("<script>alert(1)</script>"), "");
assert.equal(
  prepareDescriptionHtml("<p>ok<script>bad</script>end</p>"),
  "<p>okend</p>",
);
assert.equal(prepareDescriptionHtml('<a href="x">link</a>'), "link");
assert.equal(
  prepareDescriptionHtml("<p>A</p><ul><li>B</li></ul>"),
  "<p>A</p><ul><li>B</li></ul>",
);

// Nested residue must not leave a script opener
assert.equal(stripTagsFallback("<<script>alert(1)</script>"), "alert(1)");
assert.equal(stripTagsFallback("<p>Hello</p>"), "Hello");

console.log("safeHtml smoke checks passed");
