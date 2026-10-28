/**
 * Report summaries for the dashboard index: level counts and an error
 * signature that groups reports with the same failure.
 *
 * Input is a stored report (the enriched `/submit` object), so every field is
 * untrusted. `summarizeReport` never throws: missing or odd fields become
 * empty values, and strings are bounded before they reach the index.
 */

import { asObject, isObject, type JsonObject } from "../json.ts";

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
 * @param message - Event message (already redacted on the client).
 * @returns At most `MAX_NORMALIZED_CHARS` characters.
 */
export function normalizeMessage(message: string): string {
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
 * @param text - Input.
 * @returns 16 hex characters.
 */
async function shortHash(text: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(digest).slice(0, 8)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/**
 * Bounded single-line text from an untrusted field.
 *
 * @param value - Field value.
 * @param max - Longest result.
 * @returns The text, or an empty string when not a string.
 */
function field(value: unknown, max: number): string {
  if (typeof value !== "string") return "";
  return value
    .replace(/[\u0000-\u001f\u007f]+/g, " ")
    .trim()
    .slice(0, max);
}

/**
 * Index row for one stored report.
 *
 * The signature comes from the first ERROR event, or the first WARN event
 * when there is no error, and is null when there is neither.
 *
 * @param stored - The enriched object `/submit` wrote to B2.
 * @returns The fields the index keeps; counts are non-negative integers.
 */
export async function summarizeReport(stored: unknown): Promise<{
  version: string;
  os: string;
  arch: string;
  distro: string;
  trigger: string;
  events: number;
  errors: number;
  warnings: number;
  signature: string | null;
  signatureLabel: string | null;
  installHash: string;
}> {
  const s = asObject(stored);
  const p = asObject(s.payload);
  const events = Array.isArray(p.events) ? p.events : [];
  let errors = 0;
  let warnings = 0;
  let firstError: JsonObject | null = null;
  let firstWarn: JsonObject | null = null;
  for (const ev of events) {
    if (!isObject(ev)) continue;
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
  let signature: string | null = null;
  let signatureLabel: string | null = null;
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
    installHash:
      typeof s.client_ip_hash === "string" && /^[0-9a-f]{1,64}$/.test(s.client_ip_hash)
        ? s.client_ip_hash
        : "",
  };
}
