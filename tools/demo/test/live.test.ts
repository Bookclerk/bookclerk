// The live status: what the session object reports, when a page stops
// following it and how starts are recorded. The page-side follower
// (public/_demo/live.js) is tested in live-page.test.ts.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { recordStartup, startupEstimate } from "../src/limits.ts";
import {
  CLOSE_TOO_MANY,
  isOver,
  liveStatus,
  MAX_LIVE_SOCKETS,
  replyCloseCode,
  socketsToEvict,
} from "../src/live.ts";
import { createD1 } from "../test-support/d1.ts";

describe("liveStatus", () => {
  it("counts the seconds left by the given clock and never below zero", () => {
    const meta = { state: "ready" as const, endsAt: 1_000, version: "abc" };
    assert.deepEqual(liveStatus(meta, 400_500), {
      state: "ready",
      endsAt: 1_000,
      remaining: 599,
      version: "abc",
    });
    assert.equal(liveStatus(meta, 2_000_000).remaining, 0);
  });

  it("treats every state but starting and ready as over", () => {
    assert.equal(isOver("starting"), false);
    assert.equal(isOver("ready"), false);
    for (const state of ["ending", "ended", "failed", "none"] as const)
      assert.equal(isOver(state), true);
  });
});

describe("socket housekeeping", () => {
  it("closes just enough of the oldest sockets for a new one", () => {
    assert.equal(socketsToEvict(0), 0);
    assert.equal(socketsToEvict(MAX_LIVE_SOCKETS - 1), 0);
    assert.equal(socketsToEvict(MAX_LIVE_SOCKETS), 1);
    assert.equal(socketsToEvict(MAX_LIVE_SOCKETS + 3), 4);
  });

  it("answers a page's close with a code it may send", () => {
    for (const code of [1000, 1001, 1008, 1011, 3000, CLOSE_TOO_MANY, 4999])
      assert.equal(replyCloseCode(code), code);
    for (const code of [1004, 1005, 1006, 1015, 999, 2000, 5000, 1000.5, Number.NaN])
      assert.equal(replyCloseCode(code), 1000);
  });
});

describe("startup records", () => {
  /**
   * A database with one live session per id.
   *
   * @param ids - Session ids.
   * @param startedAt - Their start, Unix seconds.
   * @returns The database.
   */
  async function sessions(ids: string[], startedAt: number): Promise<D1Database> {
    const db = createD1();
    for (const id of ids) {
      await db
        .prepare("INSERT INTO sessions (id, ip_hash, started_at, ends_at) VALUES (?, 'h', ?, ?)")
        .bind(id, startedAt, startedAt + 1800)
        .run();
    }
    return db;
  }

  it("keeps the first record per session and drops impossible ones", async () => {
    const db = await sessions(["a", "b", "c", "d"], 1_000_000);
    await recordStartup(db, { id: "a", seconds: 10 });
    await recordStartup(db, { id: "a", seconds: 99 });
    await recordStartup(db, { id: "b", seconds: -1 });
    await recordStartup(db, { id: "c", seconds: 601 });
    await recordStartup(db, { id: "d", seconds: Number.NaN });
    const { results } = await db
      .prepare("SELECT id, startup_seconds FROM sessions ORDER BY id")
      .all();
    assert.deepEqual(
      results.map((r) => [r.id, r.startup_seconds]),
      [
        ["a", 10],
        ["b", null],
        ["c", null],
        ["d", null],
      ],
    );
  });

  it("estimates from the median of the last week's starts", async () => {
    const now = 2_000_000;
    const db = await sessions(["a", "b", "c", "d"], now - 3600);
    for (const [id, seconds] of [
      ["a", 8],
      ["b", 21],
      ["c", 23],
      ["d", 300],
    ] as const) {
      await recordStartup(db, { id, seconds });
    }
    // Median of 8, 21, 23 and 300 is 22, rounded up to 25.
    assert.equal(await startupEstimate(db, now), 25);
    // A week later none of them count any more.
    assert.equal(await startupEstimate(db, now + 8 * 86400), null);
  });
});
