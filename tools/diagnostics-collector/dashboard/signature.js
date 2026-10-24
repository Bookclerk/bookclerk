/**
 * Report summaries for the dashboard index: level counts and an error
 * signature that groups reports with the same failure.
 *
 * Input is a stored report (the enriched `/submit` object), so every field is
 * untrusted. `summarizeReport` never throws: missing or odd fields become
 * empty values, and strings are bounded before they reach the index.
 */

/** Longest normalized message kept in a signature, in characters. */
export const MAX_NORMALIZED_CHARS = 200;

/** Longest signature label stored in the index, in characters. */
export const MAX_LABEL_CHARS = 160;

/** Longest platform field (OS, arch, distro, version, trigger) stored. */
export const MAX_FIELD_CHARS = 64;

/** Longest event target used in a signature, in characters. */
export const MAX_TARGET_CHARS = 96;

/**
 * Normalizes a log message so the same failure on different machines reads
 * the same: UUIDs, long hex runs, paths, URLs, quoted strings and numbers are
 * replaced with placeholders, and whitespace is collapsed.
 *
 * @param {string} message Event message (already redacted on the client).
 * @returns {string} At most `MAX_NORMALIZED_CHARS` characters.
 */
export function normalizeMessage(message) {
  return String(message)
    .slice(0, 4 * MAX_NORMALIZED_CHARS)
    .replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/gi, "<id>")
    .replace(/\b[a-z][a-z0-9+.-]*:\/\/\S+/gi, "<url>")
    .replace(/(?:[A-Za-z]:\\|\\\\)[^\s"'`]*/g, "<path>")
    .replace(/(?:^|(?<=[\s("'=:]))(?:~|\.{1,2})?\/[^\s"'`)]+/g, "<path>")
    .replace(/"[^"]*"|'[^']*'|`[^`]*`/g, "<str>")
    .replace(/\b(?:0x[0-9a-f]+|(?=[0-9a-f]*[a-f])[0-9a-f]{8,})\b/gi, "<hex>")
    .replace(/\d+(?:\.\d+)*/g, "<n>")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, MAX_NORMALIZED_CHARS);
}

/**
 * Lowercase hex of the first 8 bytes of SHA-256 over a string.
 *
 * @param {string} text Input.
 * @returns {Promise<string>} 16 hex characters.
 */
async function shortHash(text) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest).slice(0, 8)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Bounded single-line text from an untrusted field.
 *
 * @param {unknown} value Field value.
 * @param {number} max Longest result.
 * @returns {string} The text, or an empty string when not a string.
 */
function field(value, max) {
  if (typeof value !== "string") return "";
  return value.replace(/[\u0000-\u001f\u007f]+/g, " ").trim().slice(0, max);
}

/**
 * Index row for one stored report.
 *
 * The signature comes from the first ERROR event, or the first WARN event
 * when there is no error, and is null when there is neither.
 *
 * @param {unknown} stored The enriched object `/submit` wrote to B2.
 * @returns {Promise<{
 *   version: string, os: string, arch: string, distro: string,
 *   trigger: string, events: number, errors: number, warnings: number,
 *   signature: string | null, signatureLabel: string | null,
 *   installHash: string
 * }>} The fields the index keeps; counts are non-negative integers.
 */
export async function summarizeReport(stored) {
  const s = stored && typeof stored === "object" && !Array.isArray(stored) ? stored : {};
  const p = s.payload && typeof s.payload === "object" && !Array.isArray(s.payload) ? s.payload : {};
  const events = Array.isArray(p.events) ? p.events : [];
  let errors = 0;
  let warnings = 0;
  let firstError = null;
  let firstWarn = null;
  for (const ev of events) {
    if (!ev || typeof ev !== "object") continue;
    const level = typeof ev.level === "string" ? ev.level.trim().toUpperCase() : "";
    if (level === "ERROR") {
      errors += 1;
      firstError ??= ev;
    } else if (level === "WARN" || level === "WARNING") {
      warnings += 1;
      firstWarn ??= ev;
    }
  }
  const top = firstError ?? firstWarn;
  let signature = null;
  let signatureLabel = null;
  if (top) {
    const target = field(top.target, MAX_TARGET_CHARS) || "unknown";
    const message = normalizeMessage(typeof top.message === "string" ? top.message : "");
    signature = await shortHash(`${target}\n${message}`);
    signatureLabel = `${target} · ${message}`.slice(0, MAX_LABEL_CHARS);
  }
  return {
    version: field(p.version, MAX_FIELD_CHARS),
    os: field(p.os, MAX_FIELD_CHARS),
    arch: field(p.arch, MAX_FIELD_CHARS),
    distro: field(p.distro, MAX_FIELD_CHARS),
    trigger: field(p.trigger, MAX_FIELD_CHARS),
    events: events.length,
    errors,
    warnings,
    signature,
    signatureLabel,
    installHash: /^[0-9a-f]{1,64}$/.test(s.client_ip_hash ?? "") ? s.client_ip_hash : "",
  };
}
