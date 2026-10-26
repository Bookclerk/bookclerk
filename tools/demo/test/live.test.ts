// The live status: what the session object reports, when a page stops
// following it, how starts are recorded, and the page-side follower
// (public/_demo/live.js) with and without a working WebSocket.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import { runInNewContext } from "node:vm";
import { recordStartup, startupEstimate } from "../src/limits.ts";
import { CLOSE_TOO_MANY, isOver, liveStatus, MAX_LIVE_SOCKETS, replyCloseCode, socketsToEvict } from "../src/live.ts";
import { createD1 } from "../test-support/d1.ts";

describe("liveStatus", () => {
  it("counts the seconds left by the given clock and never below zero", () => {
    const meta = { state: "ready" as const, endsAt: 1_000, version: "abc" };
    assert.deepEqual(liveStatus(meta, 400_500), { state: "ready", endsAt: 1_000, remaining: 599, version: "abc" });
    assert.equal(liveStatus(meta, 2_000_000).remaining, 0);
  });

  it("treats every state but starting and ready as over", () => {
    assert.equal(isOver("starting"), false);
    assert.equal(isOver("ready"), false);
    for (const state of ["ending", "ended", "failed", "none"] as const) assert.equal(isOver(state), true);
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
    for (const code of [1000, 1001, 1008, 1011, 3000, CLOSE_TOO_MANY, 4999]) assert.equal(replyCloseCode(code), code);
    for (const code of [1004, 1005, 1006, 1015, 999, 2000, 5000, 1000.5, Number.NaN]) assert.equal(replyCloseCode(code), 1000);
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
    const { results } = await db.prepare("SELECT id, startup_seconds FROM sessions ORDER BY id").all();
    assert.deepEqual(
      results.map((r) => [r.id, r.startup_seconds]),
      [["a", 10], ["b", null], ["c", null], ["d", null]],
    );
  });

  it("estimates from the median of the last week's starts", async () => {
    const now = 2_000_000;
    const db = await sessions(["a", "b", "c", "d"], now - 3600);
    for (const [id, seconds] of [["a", 8], ["b", 21], ["c", 23], ["d", 300]] as const) {
      await recordStartup(db, { id, seconds });
    }
    // Median of 8, 21, 23 and 300 is 22, rounded up to 25.
    assert.equal(await startupEstimate(db, now), 25);
    // A week later none of them count any more.
    assert.equal(await startupEstimate(db, now + 8 * 86400), null);
  });
});

/** A WebSocket stand-in the follower talks to. */
class FakeSocket {
  /** Every socket the page opened. */
  static opened: FakeSocket[] = [];
  /** Open, as WebSocket names it. */
  static readonly OPEN = 1;
  /** Where the page connected. */
  readonly url: string;
  /** 0 connecting, 1 open, 3 closed. */
  readyState = 0;
  /** Messages the page sent. */
  sent: string[] = [];
  /** Listeners by event. */
  listeners = new Map<string, Array<(event: { data?: unknown; code?: number }) => void>>();

  /** @param url - Where the page connects. */
  constructor(url: string) {
    this.url = url;
    FakeSocket.opened.push(this);
  }

  /**
   * @param type - Event name.
   * @param fn - Listener.
   */
  addEventListener(type: string, fn: (event: { data?: unknown; code?: number }) => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }

  /**
   * Fires an event at the page.
   *
   * @param type - Event name.
   * @param event - Event fields.
   */
  emit(type: string, event: { data?: unknown; code?: number } = {}): void {
    if (type === "open") this.readyState = 1;
    if (type === "close") this.readyState = 3;
    for (const fn of this.listeners.get(type) ?? []) fn(event);
  }

  /** @param data - What the page sends. */
  send(data: string): void {
    this.sent.push(data);
  }

  /** The page closes its end. */
  close(): void {
    this.emit("close", { code: 1000 });
  }
}

/** What a page running live.js saw. */
type Page = {
  /** Statuses delivered to the page. */
  statuses: Array<Record<string, unknown>>;
  /** /_demo/session polls made. */
  polls: number;
  /** Runs the timers the page set. */
  runTimers: () => Promise<void>;
  /** Fires visibilitychange with the page shown. */
  show: () => void;
};

/**
 * Loads live.js into a fake page and starts following.
 *
 * @param session - What /_demo/session answers.
 * @returns The page.
 */
function loadPage(session: Record<string, unknown>): Page {
  FakeSocket.opened = [];
  const page: Page = { statuses: [], polls: 0, runTimers: async () => {}, show: () => {} };
  let timers: Array<() => void> = [];
  const docListeners: Array<() => void> = [];
  const window: Record<string, unknown> = {};
  const context = {
    window,
    location: { protocol: "https:", host: "demo.example.test" },
    document: {
      visibilityState: "visible",
      addEventListener: (_type: string, fn: () => void) => docListeners.push(fn),
    },
    WebSocket: FakeSocket,
    setTimeout: (fn: () => void) => {
      timers.push(fn);
      return timers.length;
    },
    fetch: async () => {
      page.polls += 1;
      return { text: async () => JSON.stringify(session) };
    },
    JSON,
    Number,
    Math,
    Set,
  };
  runInNewContext(readFileSync(new URL("../public/_demo/live.js", import.meta.url), "utf8"), context);
  page.runTimers = async () => {
    const due = timers;
    timers = [];
    for (const fn of due) fn();
    await new Promise((resolve) => setImmediate(resolve));
  };
  page.show = () => docListeners.forEach((fn) => fn());
  const live = window.bookclerkDemoLive as { follow: (fn: (s: Record<string, unknown>) => void, o?: object) => void };
  // Copied into this realm, so deepEqual compares plain objects.
  live.follow((s) => page.statuses.push(JSON.parse(JSON.stringify(s)) as Record<string, unknown>), { maxRetryMs: 2000 });
  return page;
}

describe("live.js", () => {
  it("delivers the session's pushes and stops after the end", () => {
    const page = loadPage({});
    const socket = FakeSocket.opened[0]!;
    assert.equal(socket.url, "wss://demo.example.test/_demo/live");
    socket.emit("open");
    socket.emit("message", { data: JSON.stringify({ state: "starting", remaining: 1790, siteUrl: "https://x.test", extra: 1 }) });
    socket.emit("message", { data: "not json" });
    socket.emit("message", { data: JSON.stringify({ state: "bogus", remaining: 1 }) });
    socket.emit("message", { data: JSON.stringify({ state: "ready", remaining: "lots", siteUrl: "javascript:alert(1)" }) });
    socket.emit("message", { data: JSON.stringify({ state: "ended", remaining: 0 }) });
    socket.emit("message", { data: JSON.stringify({ state: "ready", remaining: 5 }) });
    assert.deepEqual(page.statuses, [
      { state: "starting", remaining: 1790, siteUrl: "https://x.test" },
      { state: "ready", remaining: 0, siteUrl: "" },
      { state: "ended", remaining: 0, siteUrl: "" },
    ]);
    assert.equal(socket.readyState, 3);
  });

  it("asks again over the socket when the page is shown", () => {
    const page = loadPage({});
    const socket = FakeSocket.opened[0]!;
    socket.emit("open");
    page.show();
    assert.deepEqual(socket.sent, ["sync"]);
    assert.equal(page.polls, 0);
  });

  it("polls the session when the socket cannot open, and keeps trying it", async () => {
    const page = loadPage({ state: "starting", remaining: 1700, siteUrl: "" });
    FakeSocket.opened[0]!.emit("close", { code: 1006 });
    await page.runTimers();
    assert.equal(page.polls >= 1, true);
    assert.deepEqual(page.statuses[0], { state: "starting", remaining: 1700, siteUrl: "" });
    // The retry polled again and opened a new socket; when that one fails
    // too, the page polls and tries once more.
    assert.equal(FakeSocket.opened.length, 2);
    const polls = page.polls;
    FakeSocket.opened[1]!.emit("close", { code: 1006 });
    await page.runTimers();
    assert.equal(page.polls > polls, true);
    assert.equal(FakeSocket.opened.length, 3);
  });

  it("polls instead of reconnecting once newer tabs took its socket", async () => {
    const page = loadPage({ state: "ready", remaining: 900, siteUrl: "" });
    const socket = FakeSocket.opened[0]!;
    socket.emit("open");
    socket.emit("close", { code: CLOSE_TOO_MANY });
    await page.runTimers();
    await page.runTimers();
    assert.equal(FakeSocket.opened.length, 1);
    assert.equal(page.polls, 2);
    assert.deepEqual(page.statuses.at(-1), { state: "ready", remaining: 900, siteUrl: "" });
  });
});
