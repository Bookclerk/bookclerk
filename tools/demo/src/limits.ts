// Admission and bookkeeping in the demo's D1 database (migrations/): how
// many sessions run at once, the daily visitor-hour budget, per-address
// rates, and store sign-in attempts. D1 has no interactive transactions, so
// two admissions racing for the last slot can both pass; the container
// class's max_instances in wrangler.toml is the hard cap behind this one.

import type { Env } from "./env.ts";

/** Admission limits, read from the Worker's vars. */
export type Limits = {
  /** Sessions that may run at once. */
  maxLive: number;
  /** Visitor-seconds per UTC day. */
  dailyBudgetSeconds: number;
  /** Hard length of one session. */
  sessionSeconds: number;
  /** New sessions per address per UTC hour. */
  sessionsPerHour: number;
  /** Store sign-in attempts per session. */
  signinsPerSession: number;
  /** Store sign-in attempts per address per UTC day. */
  signinsPerDay: number;
};

export const DEFAULT_LIMITS: Limits = Object.freeze({
  maxLive: 20,
  dailyBudgetSeconds: 40 * 3600,
  sessionSeconds: 30 * 60,
  sessionsPerHour: 3,
  signinsPerSession: 5,
  signinsPerDay: 10,
});

/**
 * Reads one positive integer var, falling back to a default.
 *
 * @param raw - Var value (a string from wrangler.toml).
 * @param fallback - Default.
 * @returns The value when it is a positive safe integer; otherwise the default.
 */
function positive(raw: unknown, fallback: number): number {
  const n = typeof raw === "string" && /^[0-9]{1,9}$/.test(raw) ? Number(raw) : NaN;
  return Number.isSafeInteger(n) && n > 0 ? n : fallback;
}

/**
 * Builds the limits from the Worker's env.
 *
 * @param env - Worker env (`MAX_LIVE_SESSIONS`,
 *   `DAILY_BUDGET_HOURS`, `SESSION_MINUTES`, `SESSIONS_PER_HOUR`,
 *   `SIGNINS_PER_SESSION`, `SIGNINS_PER_DAY`).
 * @returns Limits, with defaults for missing or malformed vars.
 */
export function limitsFrom(env: Env): Limits {
  const d = DEFAULT_LIMITS;
  return {
    maxLive: positive(env.MAX_LIVE_SESSIONS, d.maxLive),
    dailyBudgetSeconds: positive(env.DAILY_BUDGET_HOURS, d.dailyBudgetSeconds / 3600) * 3600,
    sessionSeconds: positive(env.SESSION_MINUTES, d.sessionSeconds / 60) * 60,
    sessionsPerHour: positive(env.SESSIONS_PER_HOUR, d.sessionsPerHour),
    signinsPerSession: positive(env.SIGNINS_PER_SESSION, d.signinsPerSession),
    signinsPerDay: positive(env.SIGNINS_PER_DAY, d.signinsPerDay),
  };
}

/**
 * The UTC day of a Unix time, as `YYYY-MM-DD`.
 *
 * @param seconds - Unix seconds.
 * @returns The day.
 */
export function utcDay(seconds: number): string {
  return new Date(seconds * 1000).toISOString().slice(0, 10);
}

/**
 * Bumps a per-address counter and returns the new count.
 *
 * @param db - Database.
 * @param ipHash - Address HMAC.
 * @param kind - Counter.
 * @param windowStart - Window start in Unix seconds.
 * @returns The count after this bump.
 */
async function bump(db: D1Database, ipHash: string, kind: "session" | "signin", windowStart: number): Promise<number> {
  const row = await db
    .prepare(
      `INSERT INTO rate (ip_hash, kind, window_start, count) VALUES (?, ?, ?, 1)
       ON CONFLICT (ip_hash, kind, window_start) DO UPDATE SET count = count + 1
       RETURNING count`,
    )
    .bind(ipHash, kind, windowStart)
    .first();
  return Number(row?.count ?? Infinity);
}

/**
 * Admits a new session, or says why not.
 *
 * Checks, in order: the address's hourly session rate (counted even when
 * refused, so hammering the button does not help), the number of live
 * sessions, and the day's budget, which the session reserves in full.
 *
 * @param db - Database.
 * @param s - The
 *   session id, address HMAC, current Unix seconds and image version.
 * @param limits - Limits.
 * @returns The hard stop in Unix seconds, or the refusal.
 */
export async function admit(db: D1Database, s: { id: string; ipHash: string; now: number; version: string; }, limits: Limits): Promise<{ ok: true; endsAt: number; } | { ok: false; reason: "rate" | "busy" | "budget"; }> {
  const hour = s.now - (s.now % 3600);
  if ((await bump(db, s.ipHash, "session", hour)) > limits.sessionsPerHour) {
    return { ok: false, reason: "rate" };
  }
  const live = await db
    .prepare("SELECT COUNT(*) AS n FROM sessions WHERE ended_at IS NULL AND ends_at > ?")
    .bind(s.now)
    .first();
  if (Number(live?.n ?? Infinity) >= limits.maxLive) return { ok: false, reason: "busy" };
  const reserved = await db
    .prepare(
      `INSERT INTO budget (day, reserved) SELECT ?1, ?2 WHERE ?2 <= ?3
       ON CONFLICT (day) DO UPDATE SET reserved = reserved + ?2 WHERE reserved + ?2 <= ?3
       RETURNING reserved`,
    )
    .bind(utcDay(s.now), limits.sessionSeconds, limits.dailyBudgetSeconds)
    .first();
  if (!reserved) return { ok: false, reason: "budget" };
  const endsAt = s.now + limits.sessionSeconds;
  await db
    .prepare("INSERT INTO sessions (id, ip_hash, started_at, ends_at, version) VALUES (?, ?, ?, ?, ?)")
    .bind(s.id, s.ipHash, s.now, endsAt, s.version)
    .run();
  return { ok: true, endsAt };
}

/**
 * Counts one store sign-in attempt, or refuses it.
 *
 * @param db - Database.
 * @param s - Session id, address
 *   HMAC and current Unix seconds.
 * @param limits - Limits.
 * @returns True when the attempt is within both the
 *   session's and the address's daily allowance.
 */
export async function countSignin(db: D1Database, s: { id: string; ipHash: string; now: number; }, limits: Limits): Promise<boolean> {
  const row = await db
    .prepare(
      `UPDATE sessions SET signins = signins + 1
       WHERE id = ? AND ended_at IS NULL AND signins < ? RETURNING signins`,
    )
    .bind(s.id, limits.signinsPerSession)
    .first();
  if (!row) return false;
  const day = s.now - (s.now % 86400);
  return (await bump(db, s.ipHash, "signin", day)) <= limits.signinsPerDay;
}

/**
 * Records a session's end and returns its unused time to the day's budget.
 *
 * @param db - Database.
 * @param e - Session id, current Unix seconds, why it ended, and how many store
 *   accounts were signed out and failed to sign out.
 * @returns True when this call ended the session; false
 *   when it had already ended or does not exist.
 */
export async function finish(db: D1Database, e: { id: string; now: number; reason: string; revoked: number; failed: number; }): Promise<boolean> {
  const row = await db
    .prepare(
      `UPDATE sessions SET ended_at = ?, end_reason = ?, revoked = ?, revoke_failed = ?
       WHERE id = ? AND ended_at IS NULL RETURNING started_at, ends_at`,
    )
    .bind(e.now, e.reason, e.revoked, e.failed, e.id)
    .first();
  if (!row) return false;
  const unused = Math.max(0, Number(row.ends_at) - e.now);
  if (unused > 0) {
    await db
      .prepare("UPDATE budget SET reserved = MAX(0, reserved - ?) WHERE day = ?")
      .bind(unused, utcDay(Number(row.started_at)))
      .run();
  }
  return true;
}

/** Most recent starts the estimate looks at. */
const STARTUP_SAMPLE = 25;

/** Fewest recorded starts before the pages show an estimate. */
const STARTUP_MIN_SAMPLES = 3;

/** How far back the estimate looks: older starts ran an older image. */
const STARTUP_WINDOW_SECONDS = 7 * 86400;

/** Longest start worth recording; anything longer timed out in practice. */
const STARTUP_MAX_SECONDS = 600;

/**
 * Records how long a session took to start.
 *
 * @param db - Database.
 * @param s - Session id and the seconds from Start to ready.
 */
export async function recordStartup(db: D1Database, s: { id: string; seconds: number; }): Promise<void> {
  if (!Number.isFinite(s.seconds) || s.seconds < 0 || s.seconds > STARTUP_MAX_SECONDS) return;
  await db
    .prepare("UPDATE sessions SET startup_seconds = ? WHERE id = ? AND startup_seconds IS NULL")
    .bind(Math.round(s.seconds), s.id)
    .run();
}

/**
 * How long a start usually takes: the median of the last
 * {@link STARTUP_SAMPLE} recorded starts within a week, rounded up to five
 * seconds. The median keeps the slow first start after a deploy (Cloudflare
 * pulling the new image) from moving the estimate.
 *
 * @param db - Database.
 * @param now - Current Unix seconds.
 * @returns Seconds, or null with fewer than {@link STARTUP_MIN_SAMPLES} starts.
 */
export async function startupEstimate(db: D1Database, now: number): Promise<number | null> {
  const { results } = await db
    .prepare(
      `SELECT startup_seconds FROM sessions WHERE startup_seconds IS NOT NULL AND started_at > ?
       ORDER BY started_at DESC LIMIT ?`,
    )
    .bind(now - STARTUP_WINDOW_SECONDS, STARTUP_SAMPLE)
    .all();
  const seconds = results.map((r) => Number(r.startup_seconds)).filter(Number.isFinite).sort((a, b) => a - b);
  if (seconds.length < STARTUP_MIN_SAMPLES) return null;
  const mid = Math.floor(seconds.length / 2);
  const median = seconds.length % 2 === 1 ? (seconds[mid] ?? 0) : ((seconds[mid - 1] ?? 0) + (seconds[mid] ?? 0)) / 2;
  return Math.max(5, Math.ceil(median / 5) * 5);
}

/**
 * Sessions still marked live well after their hard stop, for the sweep.
 *
 * @param db - Database.
 * @param now - Current Unix seconds.
 * @param graceSeconds - How far past the hard stop a session must be.
 * @param max - Most sessions to return.
 * @returns Session ids and
 *   their hard stops (Unix seconds), oldest first.
 */
export async function overdue(db: D1Database, now: number, graceSeconds: number, max: number): Promise<Array<{ id: string; endsAt: number; }>> {
  const { results } = await db
    .prepare("SELECT id, ends_at FROM sessions WHERE ended_at IS NULL AND ends_at < ? ORDER BY ends_at LIMIT ?")
    .bind(now - graceSeconds, max)
    .all();
  return results.map((r) => ({ id: String(r.id), endsAt: Number(r.ends_at) }));
}

/**
 * Deletes rate rows older than two days and sessions older than 30 days.
 *
 * @param db - Database.
 * @param now - Current Unix seconds.
 */
export async function prune(db: D1Database, now: number): Promise<void> {
  await db.batch([
    db.prepare("DELETE FROM rate WHERE window_start < ?").bind(now - 2 * 86400),
    db.prepare("DELETE FROM sessions WHERE started_at < ? AND ended_at IS NOT NULL").bind(now - 30 * 86400),
    db.prepare("DELETE FROM budget WHERE day < ?").bind(utcDay(now - 30 * 86400)),
  ]);
}
