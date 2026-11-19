/**
 * Opt-in usage check-ins (`POST /checkin`) and the install key reports carry.
 *
 * A Bookclerk host with `diagnostics.share_usage` on sends one check-in a
 * week: a random install ID, its version, platform and kind. The collector
 * never stores that ID. It keeps `SHA-256(month salt ‖ id)`, the
 * **install key**, where the salt is 32 random bytes made for the current
 * UTC month and deleted when the month ends. Installs can then be counted
 * per week and per month without being followed from one month to the next.
 * Check-in rows older than {@link RETENTION_MS} (by UTC day) are pruned by
 * the cron.
 *
 * A report sent with an install ID gets the same key (`/submit` swaps the ID
 * for it before the report is stored), so the dashboard can say how many
 * installs a signature came from.
 *
 * The body is untrusted (the route is open, like `/submit`), so it is parsed
 * strictly by {@link parseCheckin}, a sans-IO function fuzzed in
 * `checkin.fuzz.test.ts`, and a daily cap bounds the D1 writes a flood can
 * cause.
 */

import { all, first, run } from "./dashboard/db.ts";
import { isObject } from "./json.ts";

/** Longest check-in body accepted, in characters. */
export const MAX_CHECKIN_CHARS = 1024;

/** Kinds of install that check in. Demo copies never do. */
export const CHECKIN_KINDS = ["daemon", "desktop"] as const;

/** One of {@link CHECKIN_KINDS}. */
export type CheckinKind = (typeof CHECKIN_KINDS)[number];

/** Default most check-ins stored per UTC day (`CHECKIN_DAILY_ROWS`). */
export const DEFAULT_CHECKIN_DAILY_ROWS = 5000;

/** How long check-in rows are kept: 35 days, in milliseconds. */
export const RETENTION_MS = 35 * 86_400_000;

/** One week in milliseconds; check-ins are bucketed by Unix week. */
const WEEK_MS = 7 * 86_400_000;

/** A check-in that passed {@link parseCheckin}. */
export type Checkin = {
  /** The install's random ID: a lowercase hyphenated version-4 UUID. */
  install_id: string;
  /** Bookclerk version: 1–64 printable ASCII characters. */
  version: string;
  /** Operating system: 1–32 of `[a-z0-9_]`. */
  os: string;
  /** CPU architecture: 1–32 of `[a-z0-9_]`. */
  arch: string;
  /** What kind of install sent it. */
  kind: CheckinKind;
};

/** A version-4 UUID as the client writes it. */
const INSTALL_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** A platform token: operating system or architecture. */
const PLATFORM_RE = /^[a-z0-9_]{1,32}$/;

/** A version: printable ASCII without spaces. */
const VERSION_RE = /^[!-~]{1,64}$/;

/**
 * Whether a value is a well-formed install ID.
 *
 * @param value - Candidate.
 * @returns True for a lowercase hyphenated version-4 UUID.
 */
export function isInstallId(value: unknown): value is string {
  return typeof value === "string" && INSTALL_ID_RE.test(value);
}

/**
 * Parses a check-in body.
 *
 * @param text - The request body.
 * @returns The check-in, or null unless the body is a JSON object of at most
 *   {@link MAX_CHECKIN_CHARS} characters with exactly the fields of
 *   {@link Checkin}, each well-formed. Extra fields are refused, so a client
 *   cannot slip anything else into storage.
 */
export function parseCheckin(text: unknown): Checkin | null {
  if (typeof text !== "string" || text.length > MAX_CHECKIN_CHARS) return null;
  let data: unknown;
  try {
    data = JSON.parse(text);
  } catch {
    return null;
  }
  if (!isObject(data)) return null;
  const keys = Object.keys(data).sort();
  if (keys.join(",") !== "arch,install_id,kind,os,version") return null;
  const { install_id, version, os, arch, kind } = data;
  if (!isInstallId(install_id)) return null;
  if (typeof version !== "string" || !VERSION_RE.test(version)) return null;
  if (typeof os !== "string" || !PLATFORM_RE.test(os)) return null;
  if (typeof arch !== "string" || !PLATFORM_RE.test(arch)) return null;
  if (kind !== "daemon" && kind !== "desktop") return null;
  return { install_id, version, os, arch, kind };
}

/**
 * The UTC month of a time.
 *
 * @param ms - Unix time in milliseconds.
 * @returns `YYYY-MM`.
 */
export function monthOf(ms: number): string {
  return new Date(ms).toISOString().slice(0, 7);
}

/**
 * The Unix week of a time (weeks since the epoch).
 *
 * @param ms - Unix time in milliseconds.
 * @returns The week number.
 */
export function weekOf(ms: number): number {
  return Math.floor(ms / WEEK_MS);
}

/**
 * The UTC day of a time (days since the epoch).
 *
 * @param ms - Unix time in milliseconds.
 * @returns The day number.
 */
export function dayOf(ms: number): number {
  return Math.floor(ms / 86_400_000);
}

/**
 * Lowercase hex of bytes.
 *
 * @param bytes - Bytes.
 * @returns Hex text.
 */
function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * The current month's salt, made on first use. Salts of other months are
 * deleted by {@link pruneUsage}.
 *
 * @param db - Dashboard database.
 * @param now - Current time, Unix ms.
 * @returns The salt as hex.
 */
async function monthSalt(db: D1Database, now: number): Promise<string> {
  const month = monthOf(now);
  const fresh = hex(crypto.getRandomValues(new Uint8Array(32)));
  // A concurrent first use keeps whichever salt landed first.
  await run(db, "INSERT OR IGNORE INTO install_salts (month, salt) VALUES (?, ?)", month, fresh);
  const row = await first(db, "SELECT salt FROM install_salts WHERE month = ?", month);
  return typeof row?.salt === "string" ? row.salt : fresh;
}

/**
 * The install key for an install ID this month.
 *
 * @param db - Dashboard database.
 * @param installId - A well-formed install ID ({@link isInstallId}).
 * @param now - Current time, Unix ms.
 * @returns `SHA-256(salt ‖ id)` as 64 hex characters.
 */
export async function installKey(db: D1Database, installId: string, now: number): Promise<string> {
  const salt = await monthSalt(db, now);
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`${salt}\n${installId}`),
  );
  return hex(new Uint8Array(digest));
}

/**
 * Stores a check-in once per install and week, under today's cap. A repeat
 * is found by its primary key; a new check-in first reserves a slot in its
 * day's counter (`checkin_days`) in one statement, so concurrent check-ins
 * cannot pass the cap and the check costs the same however full the day is.
 *
 * @param db - Dashboard database.
 * @param c - The parsed check-in.
 * @param now - Current time, Unix ms.
 * @param dailyRows - Most check-ins stored per UTC day.
 * @returns "stored", "repeat" (already counted this week) or "capped".
 */
export async function recordCheckin(
  db: D1Database,
  c: Checkin,
  now: number,
  dailyRows: number,
): Promise<"stored" | "repeat" | "capped"> {
  const day = dayOf(now);
  const week = weekOf(now);
  const key = await installKey(db, c.install_id, now);
  const seen = await first(
    db,
    "SELECT 1 AS hit FROM checkins WHERE week = ? AND install_key = ?",
    week,
    key,
  );
  if (seen) return "repeat";
  const reserved = await db
    .prepare(
      `INSERT INTO checkin_days (day, rows) SELECT ?, 1 WHERE ? > 0
       ON CONFLICT (day) DO UPDATE SET rows = rows + 1 WHERE rows < ?`,
    )
    .bind(day, dailyRows, dailyRows)
    .run();
  if (Number(reserved.meta?.changes ?? 0) === 0) return "capped";
  const res = await db
    .prepare(
      `INSERT OR IGNORE INTO checkins (week, install_key, month, day, received_at, version, os, arch, kind)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(week, key, monthOf(now), day, now, c.version, c.os, c.arch, c.kind)
    .run();
  if (Number(res.meta?.changes ?? 0) > 0) return "stored";
  // The same install checked in concurrently: give the slot back.
  await run(db, "UPDATE checkin_days SET rows = rows - 1 WHERE day = ? AND rows > 0", day);
  return "repeat";
}

/**
 * Deletes check-ins older than {@link RETENTION_MS} and every month's salt
 * but the current one. Run by the cron.
 *
 * @param db - Dashboard database.
 * @param now - Current time, Unix ms.
 */
export async function pruneUsage(db: D1Database, now: number): Promise<void> {
  await db.batch([
    db.prepare("DELETE FROM checkins WHERE day < ?").bind(dayOf(now - RETENTION_MS)),
    db.prepare("DELETE FROM checkin_days WHERE day < ?").bind(dayOf(now - RETENTION_MS)),
    db.prepare("DELETE FROM install_salts WHERE month <> ?").bind(monthOf(now)),
  ]);
}

/** Active installs for the overview tile. */
export type UsageStats = {
  /** Distinct installs that checked in today or on the six days before. */
  weekly: number;
  /** Distinct installs that checked in during the current UTC month. */
  monthly: number;
  /** Last-7-day installs by kind. */
  byKind: Array<{ kind: string; n: number }>;
  /** Last-7-day installs by version, most first. */
  byVersion: Array<{ version: string; n: number }>;
};

/**
 * Counts active installs.
 *
 * @param db - Dashboard database.
 * @param now - Current time, Unix ms.
 * @returns The counts.
 */
export async function usageStats(db: D1Database, now: number): Promise<UsageStats> {
  // Whole UTC days, so the day index answers: today and the six before it.
  const since = dayOf(now) - 6;
  const [weekly, monthly, byKind, byVersion] = await Promise.all([
    first(db, "SELECT COUNT(DISTINCT install_key) AS n FROM checkins WHERE day >= ?", since),
    first(
      db,
      "SELECT COUNT(DISTINCT install_key) AS n FROM checkins WHERE month = ?",
      monthOf(now),
    ),
    all(
      db,
      `SELECT kind, COUNT(DISTINCT install_key) AS n FROM checkins WHERE day >= ?
       GROUP BY kind ORDER BY n DESC, kind`,
      since,
    ),
    all(
      db,
      `SELECT version, COUNT(DISTINCT install_key) AS n FROM checkins WHERE day >= ?
       GROUP BY version ORDER BY n DESC, version LIMIT 6`,
      since,
    ),
  ]);
  return {
    weekly: Number(weekly?.n ?? 0),
    monthly: Number(monthly?.n ?? 0),
    byKind: byKind.map((r) => ({ kind: String(r.kind ?? ""), n: Number(r.n) })),
    byVersion: byVersion.map((r) => ({ version: String(r.version ?? ""), n: Number(r.n) })),
  };
}

/**
 * Daily check-in cap from `CHECKIN_DAILY_ROWS`.
 *
 * @param raw - The variable's text, if set.
 * @returns A positive integer, else {@link DEFAULT_CHECKIN_DAILY_ROWS}.
 */
export function checkinDailyRows(raw: string | undefined): number {
  const n = Number(raw);
  return Number.isSafeInteger(n) && n > 0 ? n : DEFAULT_CHECKIN_DAILY_ROWS;
}
