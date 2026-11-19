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

/** Longest HTTP route kept from an event, in characters. */
export const MAX_ROUTE_CHARS = 96;

/** Event fields that name the HTTP route, in the order they are tried. */
const ROUTE_FIELDS = ["route", "http.request.route"];

/** An install key: 64 lowercase hex characters (checkin.ts). */
const INSTALL_KEY_RE = /^[0-9a-f]{64}$/;

/**
 * A structured field of an event. Clients send fields as `[name, value]`
 * pairs; anything else is ignored.
 *
 * @param ev - The event.
 * @param name - Field name.
 * @returns The first value under that name, or undefined.
 */
function eventField(ev: JsonObject, name: string): unknown {
  if (!Array.isArray(ev.fields)) return undefined;
  for (const pair of ev.fields) {
    if (Array.isArray(pair) && pair.length === 2 && pair[0] === name) return pair[1];
  }
  return undefined;
}

/**
 * The HTTP route an event names, from its `route` field or the request
 * span's copy of it.
 *
 * @param ev - The event.
 * @returns At most {@link MAX_ROUTE_CHARS} characters, or "" for none.
 */
function eventRoute(ev: JsonObject): string {
  for (const name of ROUTE_FIELDS) {
    const route = field(eventField(ev, name), MAX_ROUTE_CHARS);
    if (route) return route;
  }
  return "";
}

/**
 * The `latency_ms` an event reports.
 *
 * @param ev - The event.
 * @returns A non-negative safe integer, or null when the event has none.
 */
function eventLatency(ev: JsonObject): number | null {
  const raw = eventField(ev, "latency_ms");
  const n =
    typeof raw === "number"
      ? raw
      : typeof raw === "string" && /^\d{1,15}$/.test(raw)
        ? Number(raw)
        : NaN;
  return Number.isSafeInteger(n) && n >= 0 ? n : null;
}

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
 * when there is no error, and is null when there is neither. It covers the
 * event's target, normalized message and, when the event names one, its
 * HTTP route, so slow responses on different routes group apart.
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
  route: string;
  peakLatencyMs: number | null;
  installKey: string;
  deployment: string;
}> {
  const s = asObject(stored);
  const p = asObject(s.payload);
  const events = Array.isArray(p.events) ? p.events : [];
  let errors = 0;
  let warnings = 0;
  let firstError: JsonObject | null = null;
  let firstWarn: JsonObject | null = null;
  let peakLatencyMs: number | null = null;
  for (const ev of events) {
    if (!isObject(ev)) continue;
    const latency = eventLatency(ev);
    if (latency !== null && (peakLatencyMs === null || latency > peakLatencyMs)) {
      peakLatencyMs = latency;
    }
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
  const route = top ? eventRoute(top) : "";
  if (top) {
    const target = field(top.target, MAX_TARGET_CHARS) || "unknown";
    const message = normalizeMessage(typeof top.message === "string" ? top.message : "");
    // Without a route the hash input is what it always was, so existing
    // signatures (and the issues tracking them) stay the same.
    signature = await shortHash(
      route ? `${target}\n${message}\n${route}` : `${target}\n${message}`,
    );
    signatureLabel = (route ? `${target} · ${message} · ${route}` : `${target} · ${message}`).slice(
      0,
      MAX_LABEL_CHARS,
    );
  }
  const key =
    typeof s.install_key === "string" && INSTALL_KEY_RE.test(s.install_key) ? s.install_key : "";
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
    route,
    peakLatencyMs,
    installKey: key,
    deployment: p.deployment === "demo" ? "demo" : "",
  };
}
