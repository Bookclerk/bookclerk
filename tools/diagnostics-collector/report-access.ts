/**
 * Access codes for diagnostics reports: the credential a report's sender
 * uses to read it back or delete it (`/lookup`, lookup.ts).
 *
 * `/submit` gives each report a fresh code and answers with it; only the
 * sending install sees it. The collector keeps a SHA-256 hash of the code
 * with the report ID in D1 (`report_access`, migration 0010), never the code
 * itself, and never in the stored B2 object, so the ingest Action, triage
 * and GitHub never see it. A report ID alone (it appears in every issue's
 * hidden ID block) opens nothing.
 *
 * A code is 16 Crockford base32 characters (80 random bits), shown in
 * groups of four (`K7QM-3XRD-PW9T-H2CF`). Typed input is forgiving the way
 * Crockford's alphabet intends: case, hyphens and spaces are ignored, and
 * `O` reads as `0`, `I` and `L` as `1`.
 *
 * The parsers here are sans-IO, so the fuzz test (`report-access.fuzz.test.ts`)
 * drives the same code the routes do.
 */

import { isInstallId } from "./checkin.ts";
import { isObject } from "./json.ts";

/** Crockford's base32 alphabet: digits and letters without I, L, O and U. */
export const ACCESS_CODE_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/** Characters in a code, without the group separators. */
export const ACCESS_CODE_LENGTH = 16;

/** Longest lookup request body read, in characters. A real one is under 200. */
export const MAX_LOOKUP_CHARS = 1024;

/** How long an access row is kept, in milliseconds: the reports' 180 days plus a day. */
export const ACCESS_RETENTION_MS = 181 * 24 * 3_600_000;

/** A report ID with its access code, as a lookup or delete request names them. */
export type LookupInput = {
  /** The report's ID, a lowercase version-4 UUID. */
  report_id: string;
  /** The code, canonical: 16 uppercase alphabet characters, no separators. */
  access_code: string;
  /** Whether a form ticked the deletion confirmation (`confirm=yes`). */
  confirm: boolean;
};

/**
 * A fresh access code.
 *
 * @returns 16 alphabet characters from 80 random bits.
 */
export function newAccessCode(): string {
  const bytes = crypto.getRandomValues(new Uint8Array(10));
  let bits = 0;
  let value = 0;
  let out = "";
  for (const byte of bytes) {
    value = (value << 8) | byte;
    bits += 8;
    while (bits >= 5) {
      bits -= 5;
      out += ACCESS_CODE_ALPHABET[(value >>> bits) & 31];
    }
    value &= (1 << bits) - 1;
  }
  return out;
}

/**
 * Reads a typed access code.
 *
 * @param text - What the person typed or pasted.
 * @returns The canonical code, or null unless, after dropping hyphens and
 *   spaces, upper-casing, and reading `O` as `0` and `I`/`L` as `1`, it is
 *   exactly 16 alphabet characters.
 */
export function normalizeAccessCode(text: string): string | null {
  if (text.length > 64) return null;
  let out = "";
  for (const ch of text.toUpperCase()) {
    if (ch === "-" || ch === " ") continue;
    const c = ch === "O" ? "0" : ch === "I" || ch === "L" ? "1" : ch;
    if (!ACCESS_CODE_ALPHABET.includes(c)) return null;
    out += c;
  }
  return out.length === ACCESS_CODE_LENGTH ? out : null;
}

/**
 * A code as people read it.
 *
 * @param code - Canonical code.
 * @returns The code in hyphenated groups of four.
 */
export function formatAccessCode(code: string): string {
  return code.match(/.{1,4}/g)?.join("-") ?? code;
}

/**
 * Reads a typed report ID.
 *
 * @param text - What the person typed or pasted.
 * @returns The ID in lowercase, or null unless, trimmed, it is a version-4 UUID.
 */
export function normalizeReportId(text: string): string | null {
  const id = text.trim().toLowerCase();
  return isInstallId(id) ? id : null;
}

/**
 * Reads a lookup or delete request body.
 *
 * A JSON body (what Bookclerk sends) is an object with exactly the string
 * fields `report_id` and `access_code`. A form body (the lookup page) has
 * `report_id` and `access_code` once each, and optionally `confirm=yes`;
 * any other field refuses it.
 *
 * @param contentType - The request's `Content-Type`, or null.
 * @param text - The body (any value; only a string is read).
 * @returns The request, or null when it is refused.
 */
export function parseLookupInput(contentType: string | null, text: unknown): LookupInput | null {
  if (typeof text !== "string" || text.length > MAX_LOOKUP_CHARS) return null;
  const type = (contentType ?? "").split(";")[0]?.trim().toLowerCase() ?? "";
  let id: unknown;
  let code: unknown;
  let confirm = false;
  if (type === "application/json") {
    let data: unknown;
    try {
      data = JSON.parse(text);
    } catch {
      return null;
    }
    if (!isObject(data)) return null;
    const keys = Object.keys(data);
    if (keys.length !== 2 || !keys.includes("report_id") || !keys.includes("access_code")) {
      return null;
    }
    id = data.report_id;
    code = data.access_code;
  } else if (type === "application/x-www-form-urlencoded") {
    const seen = new Set<string>();
    for (const [key, value] of new URLSearchParams(text)) {
      if (seen.has(key)) return null;
      seen.add(key);
      if (key === "report_id") id = value;
      else if (key === "access_code") code = value;
      else if (key === "confirm" && value === "yes") confirm = true;
      else return null;
    }
  } else {
    return null;
  }
  if (typeof id !== "string" || typeof code !== "string") return null;
  const reportId = normalizeReportId(id);
  const accessCode = normalizeAccessCode(code);
  if (reportId === null || accessCode === null) return null;
  return { report_id: reportId, access_code: accessCode, confirm };
}

/**
 * The stored form of a code.
 *
 * @param reportId - The report it opens.
 * @param code - Canonical code.
 * @returns Lowercase hex SHA-256 of `<report_id>:<code>`. Binding the ID in
 *   means a hash is good for its own report only.
 */
export async function accessHash(reportId: string, code: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`${reportId}:${code}`),
  );
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * Compares two hex hashes in constant time.
 *
 * @param a - One hash.
 * @param b - The other.
 * @returns Whether they are equal.
 */
function sameHash(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/** One report's access row. */
export type AccessRow = {
  /** The report's ID. */
  report_id: string;
  /** B2 object key of the stored report. */
  object_key: string;
  /** When the report arrived, Unix ms. */
  created_at: number;
  /** When its sender deleted it, Unix ms, or null while it is stored. */
  deleted_at: number | null;
};

/**
 * The `index_days` key of a UTC day's access-code count, kept beside the
 * day's index count (`YYYY-MM-DD`) so neither spends the other's slots.
 *
 * @param day - The UTC day, `YYYY-MM-DD`.
 * @returns The counter's key.
 */
export function accessCounterDay(day: string): string {
  return `${day} access`;
}

/**
 * Records a new report's code, unless today's cap on codes is reached.
 *
 * Codes have their own daily count, capped like the dashboard's index rows
 * (`DASHBOARD_INDEX_DAILY_ROWS`), so an open `/submit` cannot spend the D1
 * write allowance through them either. The slot is reserved atomically
 * before the row is written (the same pattern as the index's
 * `reserveIndex`), so overlapping submissions cannot all slip under the
 * cap. Past the cap the report is stored as usual, only without a code.
 *
 * @param db - Dashboard database.
 * @param row - The report and when it arrived.
 * @param code - Its canonical code.
 * @param cap - Codes the collector may issue per UTC day.
 * @returns Whether the code was stored (and may be handed out).
 */
export async function storeAccess(
  db: D1Database,
  row: { report_id: string; object_key: string; created_at: number },
  code: string,
  cap: number,
): Promise<boolean> {
  if (!(cap > 0)) return false;
  const counter = accessCounterDay(new Date(row.created_at).toISOString().slice(0, 10));
  const reserved = await db
    .prepare(
      `INSERT INTO index_days (day, rows) VALUES (?, 1)
       ON CONFLICT (day) DO UPDATE SET rows = rows + 1 WHERE rows < ?`,
    )
    .bind(counter, cap)
    .run();
  if (Number(reserved.meta.changes ?? 0) === 0) return false;
  let stored = false;
  try {
    const res = await db
      .prepare(
        `INSERT INTO report_access (report_id, code_hash, object_key, created_at)
         VALUES (?, ?, ?, ?) ON CONFLICT (report_id) DO NOTHING`,
      )
      .bind(row.report_id, await accessHash(row.report_id, code), row.object_key, row.created_at)
      .run();
    stored = Number(res.meta.changes ?? 0) === 1;
  } finally {
    if (!stored) {
      // Nothing written: hand the slot back.
      await db
        .prepare("UPDATE index_days SET rows = rows - 1 WHERE day = ? AND rows > 0")
        .bind(counter)
        .run();
    }
  }
  return stored;
}

/**
 * Finds the report a code opens.
 *
 * @param db - Dashboard database.
 * @param input - Report ID and code.
 * @returns The report's access row, or null when there is none or the code
 *   does not match; the two cases are not told apart.
 */
export async function checkAccess(db: D1Database, input: LookupInput): Promise<AccessRow | null> {
  const row = await db
    .prepare(
      "SELECT report_id, code_hash, object_key, created_at, deleted_at FROM report_access WHERE report_id = ?",
    )
    .bind(input.report_id)
    .first<AccessRow & { code_hash: string }>();
  // Hashed either way, so a missing row takes as long as a wrong code.
  const hash = await accessHash(input.report_id, input.access_code);
  if (!row || !sameHash(String(row.code_hash), hash)) return null;
  return {
    report_id: String(row.report_id),
    object_key: String(row.object_key),
    created_at: Number(row.created_at),
    deleted_at: row.deleted_at === null ? null : Number(row.deleted_at),
  };
}

/**
 * Notes that a report's sender deleted its stored copy. The dashboard's
 * index row and every issue link stay, so counts and tracking are unchanged.
 *
 * @param db - Dashboard database.
 * @param reportId - The report.
 * @param now - Unix ms.
 */
export async function markDeleted(db: D1Database, reportId: string, now: number): Promise<void> {
  await db.batch([
    db
      .prepare("UPDATE report_access SET deleted_at = ? WHERE report_id = ? AND deleted_at IS NULL")
      .bind(now, reportId),
    db
      .prepare("UPDATE reports SET deleted_at = ? WHERE report_id = ? AND deleted_at IS NULL")
      .bind(now, reportId),
  ]);
}

/**
 * Drops access rows older than the reports they open.
 *
 * @param db - Dashboard database.
 * @param now - Unix ms.
 * @returns Rows removed.
 */
export async function pruneAccess(db: D1Database, now: number): Promise<number> {
  const res = await db
    .prepare("DELETE FROM report_access WHERE created_at < ?")
    .bind(now - ACCESS_RETENTION_MS)
    .run();
  return Number(res.meta.changes ?? 0);
}
