/** Escape plain text for HTML embedding. */
export function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const ALLOWED_TAGS = new Set([
  "P",
  "BR",
  "B",
  "STRONG",
  "I",
  "EM",
  "UL",
  "OL",
  "LI",
]);

/** Decode HTML entities (`&rsquo;`, `&#8217;`, …) into Unicode text. */
export function decodeHtmlEntities(raw: string): string {
  if (!raw.includes("&")) return raw;
  if (typeof document !== "undefined") {
    const ta = document.createElement("textarea");
    ta.innerHTML = raw;
    return ta.value;
  }
  return raw
    .replace(/&nbsp;/gi, " ")
    .replace(/&#160;/g, " ")
    .replace(/&rsquo;|&lsquo;|&#8217;|&#8216;|&#x2019;|&#x2018;/gi, "\u2019")
    .replace(/&rdquo;|&ldquo;|&#8220;|&#8221;|&#x201c;|&#x201d;/gi, "\u201d")
    .replace(/&mdash;|&#8212;/gi, "\u2014")
    .replace(/&ndash;|&#8211;/gi, "\u2013")
    .replace(/&hellip;|&#8230;/gi, "\u2026")
    .replace(/&apos;|&#39;|&#x27;/gi, "'")
    .replace(/&quot;/gi, '"')
    .replace(/&lt;/gi, "<")
    .replace(/&gt;/gi, ">")
    .replace(/&amp;/gi, "&");
}

/** One prompt/answer pair from Audible's guided questionnaire review body. */
export type GuidedReviewSection = {
  type: string;
  question?: string;
  answer: string;
};

/**
 * Best-effort parse of Audible guided review JSON:
 * `[{ "type", "question", "id", "answer" }, …]`.
 * Returns `null` when the body is not that schema.
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
 * Prepare description HTML for safe rendering: plain text is escaped, markup is
 * reduced to `p` / `br` / `b`/`strong` / `i`/`em` / lists (no attributes).
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

  if (typeof DOMParser === "undefined") {
    return escapeHtml(stripTagsFallback(trimmed));
  }

  const doc = new DOMParser().parseFromString(trimmed, "text/html");
  return serializeSafe(doc.body);
}

function serializeSafe(root: ParentNode): string {
  let out = "";
  root.childNodes.forEach((node) => {
    out += serializeNode(node);
  });
  return out.trim();
}

function serializeNode(node: Node): string {
  if (node.nodeType === Node.TEXT_NODE) {
    return escapeHtml(node.textContent ?? "");
  }
  if (node.nodeType !== Node.ELEMENT_NODE) {
    return "";
  }
  const el = node as Element;
  const tag = el.tagName.toUpperCase();
  if (tag === "SCRIPT" || tag === "STYLE" || tag === "NOSCRIPT") {
    return "";
  }
  if (tag === "BR") {
    return "<br>";
  }
  const inner = serializeSafe(el);
  if (!ALLOWED_TAGS.has(tag)) {
    return inner;
  }
  const lower = tag.toLowerCase();
  if (!inner && (tag === "P" || tag === "LI")) {
    return "";
  }
  return `<${lower}>${inner}</${lower}>`;
}

function stripTagsFallback(raw: string): string {
  return decodeHtmlEntities(
    raw
      .replace(/<br\s*\/?>/gi, "\n")
      .replace(/<\/p>/gi, "\n")
      .replace(/<[^>]+>/g, ""),
  )
    .replace(/\s+\n/g, "\n")
    .trim();
}

/** Plain-text teaser for cards (HTML stripped). */
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
