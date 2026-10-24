// Admission and bookkeeping in the demo's D1 database (migrations/): how
// many sessions run at once, the daily visitor-hour budget, per-address
// rates, and store sign-in attempts. D1 has no interactive transactions, so
// two admissions racing for the last slot can both pass; the container
// class's max_instances in wrangler.toml is the hard cap behind this one.

/**
 * Admission limits, read from the Worker's vars.
 *
 * @typedef {object} Limits
 * @property {number} maxLive Sessions that may run at once.
 * @property {number} dailyBudgetSeconds Visitor-seconds per UTC day.
 * @property {number} sessionSeconds Hard length of one session.
 * @property {number} sessionsPerHour New sessions per address per UTC hour.
 * @property {number} signinsPerSession Store sign-in attempts per session.
 * @property {number} signinsPerDay Store sign-in attempts per address per UTC day.
 */

/** @type {Limits} */
export const DEFAULT_LIMITS = Object.freeze({
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
 * @param {unknown} raw Var value (a string from wrangler.toml).
 * @param {number} fallback Default.
 * @returns {number} The value when it is a positive safe integer; otherwise the default.
 */
function positive(raw, fallback) {
  const n = typeof raw === "string" && /^[0-9]{1,9}$/.test(raw) ? Number(raw) : NaN;
  return Number.isSafeInteger(n) && n > 0 ? n : fallback;
}

/**
 * Builds the limits from the Worker's env.
 *
 * @param {Record<string, unknown>} env Worker env (`MAX_LIVE_SESSIONS`,
 *   `DAILY_BUDGET_HOURS`, `SESSION_MINUTES`, `SESSIONS_PER_HOUR`,
 *   `SIGNINS_PER_SESSION`, `SIGNINS_PER_DAY`).
 * @returns {Limits} Limits, with defaults for missing or malformed vars.
 */
export function limitsFrom(env) {
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
 * @param {number} seconds Unix seconds.
 * @returns {string} The day.
 */
export function utcDay(seconds) {
  return new Date(seconds * 1000).toISOString().slice(0, 10);
}

/**
 * Bumps a per-address counter and returns the new count.
 *
 * @param {D1Database} db Database.
 * @param {string} ipHash Address HMAC.
 * @param {"session" | "signin"} kind Counter.
 * @param {number} windowStart Window start in Unix seconds.
 * @returns {Promise<number>} The count after this bump.
 */
async function bump(db, ipHash, kind, windowStart) {
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
 * @param {D1Database} db Database.
 * @param {{id: string, ipHash: string, now: number, version: string}} s The
 *   session id, address HMAC, current Unix seconds and image version.
 * @param {Limits} limits Limits.
 * @returns {Promise<{ok: true, endsAt: number} | {ok: false, reason: "rate" | "busy" | "budget"}>}
 *   The hard stop in Unix seconds, or the refusal.
 */
export async function admit(db, s, limits) {
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
 * @param {D1Database} db Database.
 * @param {{id: string, ipHash: string, now: number}} s Session id, address
 *   HMAC and current Unix seconds.
 * @param {Limits} limits Limits.
 * @returns {Promise<boolean>} True when the attempt is within both the
 *   session's and the address's daily allowance.
 */
export async function countSignin(db, s, limits) {
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
 * @param {D1Database} db Database.
 * @param {{id: string, now: number, reason: string, revoked: number, failed: number}} e
 *   Session id, current Unix seconds, why it ended, and how many store
 *   accounts were signed out and failed to sign out.
 * @returns {Promise<boolean>} True when this call ended the session; false
 *   when it had already ended or does not exist.
 */
export async function finish(db, e) {
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

/**
 * Sessions still marked live well after their hard stop, for the sweep.
 *
 * @param {D1Database} db Database.
 * @param {number} now Current Unix seconds.
 * @param {number} graceSeconds How far past the hard stop a session must be.
 * @param {number} max Most ids to return.
 * @returns {Promise<string[]>} Session ids.
 */
export async function overdue(db, now, graceSeconds, max) {
  const { results } = await db
    .prepare("SELECT id FROM sessions WHERE ended_at IS NULL AND ends_at < ? ORDER BY ends_at LIMIT ?")
    .bind(now - graceSeconds, max)
    .all();
  return results.map((r) => String(r.id));
}

/**
 * Deletes rate rows older than two days and sessions older than 30 days.
 *
 * @param {D1Database} db Database.
 * @param {number} now Current Unix seconds.
 * @returns {Promise<void>}
 */
export async function prune(db, now) {
  await db.batch([
    db.prepare("DELETE FROM rate WHERE window_start < ?").bind(now - 2 * 86400),
    db.prepare("DELETE FROM sessions WHERE started_at < ? AND ended_at IS NOT NULL").bind(now - 30 * 86400),
    db.prepare("DELETE FROM budget WHERE day < ?").bind(utcDay(now - 30 * 86400)),
  ]);
}
