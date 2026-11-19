/**
 * Hosted demo runs, reported live by the demo Worker (`tools/demo`).
 *
 * The demo calls {@link DemoRuns.record} over a service binding (a private
 * Worker-to-Worker link: there is no public route) at three moments of
 * each run: when it starts, when the visitor's copy first answers (which
 * gives the startup time) and when it stops. Each event names the run by a
 * random run ID the demo makes for it, separate from the visitor's session,
 * and repeats the run's start and hard stop so any one event can create the
 * row. A run whose stop never arrives is closed as `missed` once it is past
 * its hard stop by {@link MISSED_GRACE_SECONDS}, so it counts as a run but
 * not toward run lengths. Open runs are the demo's live use.
 *
 * Nothing here names a visitor: no address, session or cookie reaches this
 * Worker, only times, the image version and why the run ended. Rows are
 * kept {@link RETENTION_DAYS} days for the trend charts on the dashboard's
 * Demo page.
 *
 * The event is parsed strictly by {@link parseDemoRunEvent}, a sans-IO
 * function fuzzed in `demo-runs.fuzz.test.ts`, even though only the demo
 * Worker can reach it.
 */

import { WorkerEntrypoint } from "cloudflare:workers";
import { all, first, run } from "./dashboard/db.ts";
import { announceChange } from "./dashboard/live.ts";
import type { Env } from "./env.ts";
import { isObject } from "./json.ts";

/** Why a run ended, as the demo records it (`end_reason` in its database). */
export const DEMO_END_REASONS = [
  "visitor",
  "idle",
  "time",
  "failed",
  "stopped",
  "sweep",
  "restart",
] as const;

/** One of {@link DEMO_END_REASONS}. */
export type DemoEndReason = (typeof DEMO_END_REASONS)[number];

/**
 * Longest run accepted: a day. The demo's hard stop is 30 minutes by
 * default and `SESSION_MINUTES` may raise it, but its daily budget caps
 * every run well inside a day.
 */
export const MAX_RUN_SECONDS = 24 * 3600;

/** Longest startup accepted, as the demo itself caps it. */
export const MAX_STARTUP_SECONDS = 600;

/** How late after its hard stop a stop may arrive (the demo's sweep runs every five minutes). */
export const MAX_STOP_LATE_SECONDS = 86_400;

/** How long past its hard stop an open run waits before it is closed as missed. */
export const MISSED_GRACE_SECONDS = 15 * 60;

/** How long run rows are kept, in days. */
export const RETENTION_DAYS = 400;

/** What every event repeats about its run. */
type RunFields = {
  /** The run's random ID: a lowercase hyphenated version-4 UUID. */
  run_id: string;
  /** When the run started, Unix seconds. */
  started_at: number;
  /** The run's hard stop, Unix seconds, after `started_at`. */
  ends_at: number;
  /** The demo image's version: 0–64 of `[0-9A-Za-z._+-]`. */
  version: string;
};

/** An event that passed {@link parseDemoRunEvent}. */
export type DemoRunEvent =
  | (RunFields & { kind: "start" })
  | (RunFields & { kind: "ready"; startup_seconds: number })
  | (RunFields & { kind: "stop"; ended_at: number; reason: DemoEndReason });

/** A run ID as the demo makes it. */
const RUN_ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

/** A demo image version. */
const VERSION_RE = /^[0-9A-Za-z._+-]{0,64}$/;

/** Unix seconds from 2020 to 2100: anything else is not a run time. */
const isTime = (v: unknown): v is number =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 1_577_836_800 && v < 4_102_444_800;

/** The keys each kind of event carries; any other key refuses the event. */
const KEYS: Record<DemoRunEvent["kind"], readonly string[]> = {
  start: ["kind", "run_id", "started_at", "ends_at", "version"],
  ready: ["kind", "run_id", "started_at", "ends_at", "version", "startup_seconds"],
  stop: ["kind", "run_id", "started_at", "ends_at", "version", "ended_at", "reason"],
};

/**
 * Parses one demo run event.
 *
 * @param value - The value the demo passed to {@link DemoRuns.record}.
 * @returns The event, or null unless it is an object with exactly the keys
 *   of its kind, a valid run ID and version, times in range with the hard
 *   stop after the start and at most {@link MAX_RUN_SECONDS} later, a
 *   whole startup of at most {@link MAX_STARTUP_SECONDS}, a stop between
 *   the start and {@link MAX_STOP_LATE_SECONDS} after the hard stop, and a
 *   known end reason.
 */
export function parseDemoRunEvent(value: unknown): DemoRunEvent | null {
  if (!isObject(value)) return null;
  const kind = value.kind;
  if (kind !== "start" && kind !== "ready" && kind !== "stop") return null;
  const keys = Object.keys(value);
  const wanted = KEYS[kind];
  if (keys.length !== wanted.length || !keys.every((k) => wanted.includes(k))) return null;
  const { run_id, started_at, ends_at, version } = value;
  if (typeof run_id !== "string" || !RUN_ID_RE.test(run_id)) return null;
  if (typeof version !== "string" || !VERSION_RE.test(version)) return null;
  if (!isTime(started_at) || !isTime(ends_at)) return null;
  if (ends_at <= started_at || ends_at - started_at > MAX_RUN_SECONDS) return null;
  const base = { run_id, started_at, ends_at, version };
  if (kind === "start") return { kind, ...base };
  if (kind === "ready") {
    const s = value.startup_seconds;
    if (typeof s !== "number" || !Number.isSafeInteger(s) || s < 0 || s > MAX_STARTUP_SECONDS)
      return null;
    return { kind, ...base, startup_seconds: s };
  }
  const { ended_at, reason } = value;
  if (!isTime(ended_at) || ended_at < started_at || ended_at > ends_at + MAX_STOP_LATE_SECONDS)
    return null;
  if (typeof reason !== "string" || !(DEMO_END_REASONS as readonly string[]).includes(reason))
    return null;
  return { kind, ...base, ended_at, reason: reason as DemoEndReason };
}

/**
 * Stores one event: the first event of a run creates its row, a ready
 * event fills the startup time once, and a stop fills the end once (a stop
 * that arrives after the run was closed as missed replaces that).
 *
 * @param db - Dashboard database.
 * @param e - The parsed event.
 */
export async function recordDemoRun(db: D1Database, e: DemoRunEvent): Promise<void> {
  const startup = e.kind === "ready" ? e.startup_seconds : null;
  const endedAt = e.kind === "stop" ? e.ended_at : null;
  const reason = e.kind === "stop" ? e.reason : null;
  await run(
    db,
    `INSERT INTO demo_runs (run_id, started_at, ends_at, version, startup_seconds, ended_at, end_reason)
     VALUES (?, ?, ?, ?, ?, ?, ?)
     ON CONFLICT (run_id) DO UPDATE SET
       startup_seconds = COALESCE(demo_runs.startup_seconds, excluded.startup_seconds),
       ended_at = CASE WHEN excluded.ended_at IS NOT NULL
         AND (demo_runs.ended_at IS NULL OR demo_runs.end_reason = 'missed')
         THEN excluded.ended_at ELSE demo_runs.ended_at END,
       end_reason = CASE WHEN excluded.end_reason IS NOT NULL
         AND (demo_runs.end_reason IS NULL OR demo_runs.end_reason = 'missed')
         THEN excluded.end_reason ELSE demo_runs.end_reason END`,
    e.run_id,
    e.started_at,
    e.ends_at,
    e.version,
    startup,
    endedAt,
    reason,
  );
}

/**
 * Closes runs whose stop never arrived and deletes rows past retention.
 *
 * @param db - Dashboard database.
 * @param nowSeconds - Current Unix seconds.
 * @returns How many open runs it closed as missed.
 */
export async function sweepDemoRuns(db: D1Database, nowSeconds: number): Promise<number> {
  const [closed] = await db.batch([
    db
      .prepare(
        "UPDATE demo_runs SET ended_at = ends_at, end_reason = 'missed' WHERE ended_at IS NULL AND ends_at < ?",
      )
      .bind(nowSeconds - MISSED_GRACE_SECONDS),
    db
      .prepare("DELETE FROM demo_runs WHERE started_at < ?")
      .bind(nowSeconds - RETENTION_DAYS * 86_400),
  ]);
  return closed?.meta.changes ?? 0;
}

/** One UTC day of demo runs. */
export type DemoDay = {
  /** Day index (Unix seconds / 86 400). */
  day: number;
  /** Runs started that day. */
  runs: number;
  /** Runs started that day, by how they ended (`live` while open, `missed` without a stop). */
  byReason: Record<string, number>;
  /** Seconds of use in runs that ended with a stop. */
  seconds: number;
  /** Runs that ended with a stop (the ones `seconds` counts). */
  timed: number;
  /** Sum and count of startup times recorded that day. */
  startupSum: number;
  /** Runs with a startup time. */
  startups: number;
};

/** A run that is open now. */
export type LiveRun = {
  /** When it started, Unix seconds. */
  started_at: number;
  /** Its hard stop, Unix seconds. */
  ends_at: number;
  /** The demo image's version. */
  version: string;
  /** Its startup time, or null while it starts. */
  startup_seconds: number | null;
};

/** What the Demo page shows. */
export type DemoStats = {
  /** Open runs, oldest first. */
  live: LiveRun[];
  /** One entry per day with runs, oldest first, within the range. */
  days: DemoDay[];
};

/**
 * Reads the demo's live runs and its daily figures.
 *
 * @param db - Dashboard database.
 * @param nowSeconds - Current Unix seconds.
 * @param days - Days back, today included.
 * @returns The figures.
 */
export async function demoStats(
  db: D1Database,
  nowSeconds: number,
  days: number,
): Promise<DemoStats> {
  const today = Math.floor(nowSeconds / 86_400);
  const since = (today - days + 1) * 86_400;
  const [live, byDay] = await Promise.all([
    all(
      db,
      `SELECT started_at, ends_at, version, startup_seconds FROM demo_runs
       WHERE ended_at IS NULL AND ends_at >= ? ORDER BY started_at LIMIT 100`,
      nowSeconds - MISSED_GRACE_SECONDS,
    ),
    all(
      db,
      `SELECT started_at / 86400 AS day, COALESCE(end_reason, 'live') AS reason, COUNT(*) AS n,
         COALESCE(SUM(CASE WHEN end_reason IS NOT NULL AND end_reason <> 'missed'
           THEN MIN(ended_at, ends_at) - started_at END), 0) AS seconds,
         SUM(end_reason IS NOT NULL AND end_reason <> 'missed') AS timed,
         COALESCE(SUM(startup_seconds), 0) AS startup_sum, COUNT(startup_seconds) AS startups
       FROM demo_runs WHERE started_at >= ? GROUP BY day, reason ORDER BY day`,
      since,
    ),
  ]);
  const map = new Map<number, DemoDay>();
  for (const r of byDay) {
    const day = Number(r.day);
    const d = map.get(day) ?? {
      day,
      runs: 0,
      byReason: {},
      seconds: 0,
      timed: 0,
      startupSum: 0,
      startups: 0,
    };
    const n = Number(r.n);
    d.runs += n;
    d.byReason[String(r.reason)] = (d.byReason[String(r.reason)] ?? 0) + n;
    d.seconds += Number(r.seconds);
    d.timed += Number(r.timed);
    d.startupSum += Number(r.startup_sum);
    d.startups += Number(r.startups);
    map.set(day, d);
  }
  return {
    live: live.map((r) => ({
      started_at: Number(r.started_at),
      ends_at: Number(r.ends_at),
      version: String(r.version ?? ""),
      startup_seconds: r.startup_seconds === null ? null : Number(r.startup_seconds),
    })),
    days: [...map.values()].sort((a, b) => a.day - b.day),
  };
}

/**
 * Open demo runs right now, for the overview.
 *
 * @param db - Dashboard database.
 * @param nowSeconds - Current Unix seconds.
 * @returns The count.
 */
export async function liveDemoRuns(db: D1Database, nowSeconds: number): Promise<number> {
  const row = await first(
    db,
    "SELECT COUNT(*) AS n FROM demo_runs WHERE ended_at IS NULL AND ends_at >= ?",
    nowSeconds - MISSED_GRACE_SECONDS,
  );
  return Number(row?.n ?? 0);
}

/**
 * The private entrypoint the demo Worker's `USAGE` service binding names
 * (`entrypoint = "DemoRuns"`). It has no route, so only Workers bound to it
 * can call it.
 */
export class DemoRuns extends WorkerEntrypoint<Env> {
  /**
   * Records one demo run event. A refused event or a database failure is
   * logged and dropped: the demo never waits on, or fails because of, the
   * dashboard.
   *
   * @param event - The event (see {@link parseDemoRunEvent}).
   * @returns Whether it was stored.
   */
  async record(event: unknown): Promise<boolean> {
    const db = this.env.DASHBOARD_DB;
    const parsed = parseDemoRunEvent(event);
    if (!db || !parsed) {
      if (!parsed) console.error("demo run event refused");
      return false;
    }
    try {
      await recordDemoRun(db, parsed);
      // Open /demo pages refresh their live count and run list.
      await announceChange(this.env);
      return true;
    } catch (err) {
      console.error(`demo run not stored: ${err instanceof Error ? err.message : String(err)}`);
      return false;
    }
  }
}
