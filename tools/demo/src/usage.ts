// Live demo usage: each run's start, ready and stop, reported to the
// diagnostics Worker over the `USAGE` service binding (a private
// Worker-to-Worker link; the collector's `DemoRuns` entrypoint has no
// route). The events name a run by its random run ID (the `run_id` column),
// never the visitor's session, address or cookie. Reporting is best effort:
// without the binding nothing is sent, and a failure or a slow answer is
// logged and never delays or fails the demo for longer than
// REPORT_TIMEOUT_MS.

import type { Env } from "./env.ts";

/** How a run ended, as `finish` records it. */
export type EndReason = "visitor" | "idle" | "time" | "failed" | "stopped" | "sweep";

/** What every event repeats about its run, so any one event can create it. */
export type RunFields = {
  /** The run's random ID (a version-4 UUID). */
  run_id: string;
  /** Start, Unix seconds. */
  started_at: number;
  /** Hard stop, Unix seconds. */
  ends_at: number;
  /** The demo image's version. */
  version: string;
};

/** One event the collector's `DemoRuns.record` accepts. */
export type RunEvent =
  | (RunFields & { kind: "start" })
  | (RunFields & { kind: "ready"; startup_seconds: number })
  | (RunFields & { kind: "stop"; ended_at: number; reason: EndReason });

/** The collector's entrypoint, as the service binding exposes it. */
export interface UsageService {
  /**
   * Records one event.
   *
   * @param event - The event.
   * @returns Whether the collector stored it.
   */
  record(event: RunEvent): Promise<boolean>;
}

/** Longest a report may hold up its caller. */
export const REPORT_TIMEOUT_MS = 2000;

/** The reasons `finish` may record, as the collector knows them. */
const REASONS: readonly string[] = ["visitor", "idle", "time", "failed", "stopped", "sweep"];

/**
 * Whether `reason` is one the collector accepts.
 *
 * @param reason - Why a session ended.
 * @returns True for a known reason.
 */
export function isEndReason(reason: string): reason is EndReason {
  return REASONS.includes(reason);
}

/**
 * The run fields of a session row, or null for a row without a run ID
 * (from before run IDs) or with a version the collector would refuse.
 *
 * @param row - `run_id`, `started_at`, `ends_at` and `version` of a session row.
 * @returns The fields.
 */
export function runFields(row: Record<string, unknown> | null): RunFields | null {
  if (!row) return null;
  const runId = String(row.run_id ?? "");
  const version = String(row.version ?? "").slice(0, 64);
  if (!/^[0-9a-f-]{36}$/.test(runId) || !/^[0-9A-Za-z._+-]*$/.test(version)) return null;
  return {
    run_id: runId,
    started_at: Number(row.started_at),
    ends_at: Number(row.ends_at),
    version,
  };
}

/**
 * Sends one event to the collector.
 *
 * @param env - Worker env (`USAGE`).
 * @param event - The event, or null to send nothing.
 * @returns Resolves when sent, refused, failed or timed out; never rejects.
 */
export async function reportRun(env: Pick<Env, "USAGE">, event: RunEvent | null): Promise<void> {
  const usage = env.USAGE;
  if (!usage || !event) return;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const stored = await Promise.race([
      usage.record(event),
      new Promise<"timeout">((resolve) => {
        timer = setTimeout(() => resolve("timeout"), REPORT_TIMEOUT_MS);
      }),
    ]);
    if (stored !== true) console.error(`demo ${event.kind} report not stored: ${String(stored)}`);
  } catch (err) {
    console.error(`demo ${event.kind} report failed: ${String(err)}`);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
}

/**
 * The ready event for the row `recordStartup` returned.
 *
 * @param row - The row, or null when nothing was recorded.
 * @returns The event, or null.
 */
export function readyEvent(row: Record<string, unknown> | null): RunEvent | null {
  const run = runFields(row);
  if (!run || !row) return null;
  return { kind: "ready", ...run, startup_seconds: Number(row.startup_seconds) };
}

/**
 * The stop event for the row `finish` returned.
 *
 * @param row - The row, or null when this call did not end the session.
 * @param endedAt - When it ended, Unix seconds.
 * @param reason - Why.
 * @returns The event, or null (also for a reason the collector does not know).
 */
export function stopEvent(
  row: Record<string, unknown> | null,
  endedAt: number,
  reason: string,
): RunEvent | null {
  const run = runFields(row);
  if (!run || !isEndReason(reason)) return null;
  return { kind: "stop", ...run, ended_at: endedAt, reason };
}
