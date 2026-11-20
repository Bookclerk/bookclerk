/**
 * The `POST /submit` body: a client's redacted diagnostics report.
 *
 * Sans-IO, so the fuzz test (`submit.fuzz.test.ts`) drives the same code the
 * Worker does. The Worker reads the body with `readCapped` (triage.ts) under
 * {@link MAX_SUBMIT_BYTES}, so a chunked body or an understated
 * `Content-Length` is cut off while it streams rather than buffered first,
 * then hands the text to {@link parseSubmit}.
 */

import { isObject, isRecordLike, type JsonObject } from "./json.ts";

/** Largest report body accepted, in bytes (UTF-8, as it arrives). */
export const MAX_SUBMIT_BYTES = 256 * 1024;

/** Most log events one report may carry. */
export const MAX_SUBMIT_EVENTS = 500;

/** A client report that passed {@link validateClientPayload}. */
export type ClientPayload = JsonObject & {
  /** What made the client send it: 1 to 64 characters. */
  trigger: string;
  /** The client's bookclerk version: 1 to 64 characters, not all blank. */
  version: string;
  /** The client's operating system: at most 64 characters. */
  os: string;
  /** The client's log events: at most {@link MAX_SUBMIT_EVENTS} objects. */
  events: unknown[];
};

/**
 * Parses and checks a report body.
 *
 * @param text - The body as UTF-8 text, already read under
 *   {@link MAX_SUBMIT_BYTES}.
 * @returns The report, or a short reason it is refused (the Worker answers
 *   400 with it).
 */
export function parseSubmit(text: string): ClientPayload | string {
  if (text.length === 0) return "invalid payload size";
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return "invalid json";
  }
  return validateClientPayload(parsed);
}

/**
 * Checks the shape of a client report.
 *
 * @param p - The parsed body.
 * @returns The report, or a short reason it is refused.
 */
export function validateClientPayload(p: unknown): ClientPayload | string {
  if (!isObject(p)) return "body must be an object";
  if (typeof p.trigger !== "string" || !p.trigger || p.trigger.length > 64) {
    return "trigger required";
  }
  if (typeof p.version !== "string" || !p.version.trim() || p.version.length > 64) {
    return "version required";
  }
  if (typeof p.os !== "string" || p.os.length > 64) return "os required";
  if (typeof p.archived_at_unix_ms !== "number" && typeof p.archived_at_unix_ms !== "undefined") {
    return "archived_at_unix_ms invalid";
  }
  if (!Array.isArray(p.events)) return "events must be an array";
  if (p.events.length > MAX_SUBMIT_EVENTS) return "too many events";
  for (const ev of p.events as unknown[]) {
    // Any non-null object passes, arrays included, as before the TypeScript
    // conversion; the fields checked below are read off it as a record.
    if (!isRecordLike(ev)) return "invalid event";
    if (typeof ev.message === "string" && ev.message.length > 8000) {
      return "event message too long";
    }
    if (typeof ev.level === "string" && ev.level.length > 32) return "event level too long";
    if (typeof ev.target === "string" && ev.target.length > 256) return "event target too long";
  }
  return { ...p, trigger: p.trigger, version: p.version, os: p.os, events: p.events };
}

/**
 * Whether text holds something shaped like a credential the client should
 * have redacted.
 *
 * @param text - Serialized report.
 * @returns True when any known token or key pattern matches.
 */
export function looksLikeSecretLeak(text: string): boolean {
  return (
    /\bAtna\|[A-Za-z0-9._\-+/=]+/.test(text) ||
    /\bAtnr\|[A-Za-z0-9._\-+/=]+/.test(text) ||
    /\bBearer\s+[A-Za-z0-9._\-+/=]{20,}/.test(text) ||
    /\bgh[pousr]_[A-Za-z0-9_]{20,}/.test(text) ||
    /\bgithub_pat_[A-Za-z0-9_]{20,}/.test(text) ||
    /\bAKIA[0-9A-Z]{16}\b/.test(text) ||
    /-----BEGIN [A-Z ]*PRIVATE KEY-----/.test(text)
  );
}
