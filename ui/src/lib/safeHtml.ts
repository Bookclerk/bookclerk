/**
 * Description / review HTML helpers for the operator SPA.
 *
 * ## Why string parsing instead of the DOM
 *
 * A browser `DOMParser` + tree walk (and `textarea.innerHTML` for entity
 * decode) is generally more faithful to real HTML quirks than regex/token
 * string parsing. We intentionally avoid those DOM round-trips so CodeQL
 * (`js/xss-through-dom`, `js/incomplete-multi-character-sanitization`) stays
 * green without alert dismissals: untrusted input is never assigned to
 * `innerHTML` / parsed as HTML, text is always {@link escapeHtml}'d, and only
 * constant allowlisted tag literals are emitted.
 *
 * **Tradeoff:** exotic or broken markup from storefronts may sanitize
 * differently than a full HTML parser would. If bug reports show real
 * description/review rendering problems that a DOM allowlist walk would fix,
 * prefer reverting entity decode + markup sanitization to DOM-based
 * implementations and resolving CodeQL with a well-reviewed approach (or
 * accepting a documented exception) rather than growing this string parser.
 */

/**
 * Escapes plain text for HTML embedding.
 *
 * @param raw - Untrusted text.
 * @returns Entity-escaped string safe for HTML text nodes.
 */
export function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Opening tags emitted for allowlisted elements (constant literals only). */
const ALLOWED_OPEN: Record<string, string> = {
  P: "<p>",
  B: "<b>",
  STRONG: "<strong>",
  I: "<i>",
  EM: "<em>",
  UL: "<ul>",
  OL: "<ol>",
  LI: "<li>",
};

/** Matching closing tags for {@link ALLOWED_OPEN}. */
const ALLOWED_CLOSE: Record<string, string> = {
  P: "</p>",
  B: "</b>",
  STRONG: "</strong>",
  I: "</i>",
  EM: "</em>",
  UL: "</ul>",
  OL: "</ol>",
  LI: "</li>",
};

/** Named entities commonly seen in storefront / review copy. */
const NAMED_ENTITIES: Record<string, string> = {
  amp: "&",
  lt: "<",
  gt: ">",
  quot: '"',
  apos: "'",
  nbsp: "\u00A0",
  rsquo: "\u2019",
  lsquo: "\u2018",
  rdquo: "\u201D",
  ldquo: "\u201C",
  mdash: "\u2014",
  ndash: "\u2013",
  hellip: "\u2026",
};

/**
 * Decodes HTML entities (`&rsquo;`, `&#8217;`, …) into Unicode text.
 *
 * Uses a pure string decoder (no `innerHTML` / DOM text round-trip) so the
 * result is never sourced from reinterpreted DOM HTML.
 *
 * @param raw - Text that may contain entities.
 * @returns Decoded string. Unknown named entities are left unchanged; numeric
 *   entities use {@link String.fromCodePoint} when valid.
 */
export function decodeHtmlEntities(raw: string): string {
  if (!raw.includes("&")) return raw;
  return raw.replace(
    /&(#x[0-9a-fA-F]+|#\d+|[a-zA-Z][a-zA-Z0-9]+);/g,
    (entity, body: string) => {
      if (body.startsWith("#")) {
        const code =
          body[1] === "x" || body[1] === "X"
            ? Number.parseInt(body.slice(2), 16)
            : Number.parseInt(body.slice(1), 10);
        if (!Number.isFinite(code) || code < 0 || code > 0x10ffff) {
          return entity;
        }
        try {
          return String.fromCodePoint(code);
        } catch {
          return entity;
        }
      }
      return NAMED_ENTITIES[body.toLowerCase()] ?? entity;
    },
  );
}

/**
 * One prompt/answer pair from Audible's guided questionnaire review body.
 */
export type GuidedReviewSection = {
  type: string;
  question?: string;
  answer: string;
};

/**
 * Best-effort parse of Audible guided review JSON.
 *
 * @param raw - Review body that may be a JSON array of `{type,question,answer}`.
 * @returns Parsed sections, or `null` when the body is not that schema.
 */
export function parseGuidedReviewBody(raw: string): GuidedReviewSection[] | null {
  const trimmed = raw.trim();
  if (!trimmed.startsWith("[")) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return null;
  }
  if (!Array.isArray(parsed) || parsed.length === 0) return null;

  const sections: GuidedReviewSection[] = [];
  let guidedHits = 0;
  for (const item of parsed) {
    if (!item || typeof item !== "object") continue;
    const obj = item as Record<string, unknown>;
    const hasAnswer = "answer" in obj;
    const hasPrompt = "question" in obj || "type" in obj;
    if (!(hasAnswer && hasPrompt)) continue;
    guidedHits += 1;
    const answer =
      typeof obj.answer === "string"
        ? decodeHtmlEntities(obj.answer).trim()
        : "";
    if (!answer) continue;
    const type =
      typeof obj.type === "string" ? decodeHtmlEntities(obj.type).trim() : "";
    const question =
      typeof obj.question === "string"
        ? decodeHtmlEntities(obj.question).trim()
        : undefined;
    sections.push({ type, question: question || undefined, answer });
  }

  if (guidedHits === 0 || guidedHits * 2 < parsed.length) return null;
  return sections.length > 0 ? sections : null;
}

/**
 * Prepares description HTML for safe rendering.
 *
 * Plain text is escaped; markup is reduced to `p` / `br` / `b`/`strong` /
 * `i`/`em` / lists (no attributes). Guided JSON bodies return empty string
 * (render structured React instead).
 *
 * Uses {@link sanitizeMarkupToAllowlist} (string token walk). See the module
 * comment for why we avoid `DOMParser` / `innerHTML` and when to reconsider.
 *
 * @param raw - Store or review description.
 * @returns Sanitized HTML fragment.
 */
export function prepareDescriptionHtml(raw: string): string {
  // Guided JSON is rendered as structured React — never as escaped prose.
  if (parseGuidedReviewBody(raw)) return "";

  const decoded = decodeHtmlEntities(raw.trim());
  const trimmed = decoded.trim();
  if (!trimmed) return "";

  // Plain text (no markup): escape and preserve paragraphs/newlines.
  if (!/<[a-zA-Z!/?]/.test(trimmed)) {
    return escapeHtml(trimmed).replace(/\r\n|\r|\n/g, "<br>");
  }

  return sanitizeMarkupToAllowlist(trimmed);
}

/**
 * Rebuilds HTML from decoded markup by emitting only allowlisted tags.
 *
 * Text is always escaped. Tags outside the allowlist are dropped (unwrap).
 * `script` / `style` / `noscript` drop both the tags and their enclosed text,
 * matching the previous DOM walker.
 *
 * @param decoded - Entity-decoded markup (may contain untrusted tags).
 * @returns Allowlisted HTML fragment safe for `dangerouslySetInnerHTML`.
 */
function sanitizeMarkupToAllowlist(decoded: string): string {
  const tokenRe = /<\/?([a-zA-Z][a-zA-Z0-9]*)\b[^>]*>/g;
  let out = "";
  let last = 0;
  let skipUntil: string | null = null;

  for (const match of decoded.matchAll(tokenRe)) {
    const idx = match.index ?? 0;
    const token = match[0];
    const tag = match[1].toUpperCase();
    const closing = token.startsWith("</");

    if (skipUntil !== null) {
      if (closing && tag === skipUntil) {
        skipUntil = null;
      }
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

/**
 * Strips markup for plain-text teasers.
 *
 * Loops each replacement to a fixed point so nested residue like `<<script>`
 * cannot re-form a dangerous opener after a single pass (CodeQL
 * `js/incomplete-multi-character-sanitization`).
 *
 * @param raw - HTML or plain text.
 * @returns Plain text with tags removed.
 */
function stripTagsFallback(raw: string): string {
  let s = decodeHtmlEntities(raw);

  s = replaceToFixedPoint(s, /<br\s*\/?>/gi, "\n");
  s = replaceToFixedPoint(s, /<\/p>/gi, "\n");
  // Match tags that cannot themselves contain `<` / `>` (avoids the classic
  // single-pass `/<[^>]*>/` residue issue).
  s = replaceToFixedPoint(s, /<[^<>]*>/g, "");
  // Drop any leftover angle brackets so a truncated opener cannot survive.
  s = s.replace(/[<>]/g, "");

  return s.replace(/\s+\n/g, "\n").trim();
}

/**
 * Applies `pattern` → `replacement` until the string stops changing.
 *
 * @param input - Subject string.
 * @param pattern - Global regex to apply.
 * @param replacement - Replacement string.
 * @returns Stable result after repeated replacement.
 */
function replaceToFixedPoint(
  input: string,
  pattern: RegExp,
  replacement: string,
): string {
  let previous: string;
  let current = input;
  do {
    previous = current;
    current = current.replace(pattern, replacement);
  } while (current !== previous);
  return current;
}

/**
 * Builds a plain-text teaser for cards (HTML stripped).
 *
 * @param raw - Optional HTML or guided-review JSON body.
 * @returns Single-line plain text.
 */
export function descriptionPlainText(raw: string | null | undefined): string {
  if (!raw?.trim()) return "";
  const guided = parseGuidedReviewBody(raw);
  if (guided) {
    return guided
      .map((s) => stripTagsFallback(s.answer))
      .join(" ")
      .replace(/\s+/g, " ")
      .trim();
  }
  return stripTagsFallback(raw).replace(/\s+/g, " ").trim();
}
