// The page-side live status follower (public/_demo/live.js), with and
// without a working WebSocket. It is a browser script, so it runs in a
// node:vm context with stand-ins for the page, outside the Workers runtime.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { URL } from "node:url";
import { describe, it } from "vitest";
import { CLOSE_TOO_MANY } from "../src/live.ts";

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
  runInNewContext(
    readFileSync(new URL("../public/_demo/live.js", import.meta.url), "utf8"),
    context,
  );
  page.runTimers = async () => {
    const due = timers;
    timers = [];
    for (const fn of due) fn();
    await new Promise((resolve) => setImmediate(resolve));
  };
  page.show = () => docListeners.forEach((fn) => fn());
  const live = window.bookclerkDemoLive as {
    follow: (fn: (s: Record<string, unknown>) => void, o?: object) => void;
  };
  // Copied into this realm, so deepEqual compares plain objects.
  live.follow((s) => page.statuses.push(JSON.parse(JSON.stringify(s)) as Record<string, unknown>), {
    maxRetryMs: 2000,
  });
  return page;
}

describe("live.js", () => {
  it("delivers the session's pushes and stops after the end", () => {
    const page = loadPage({});
    const socket = FakeSocket.opened[0]!;
    assert.equal(socket.url, "wss://demo.example.test/_demo/live");
    socket.emit("open");
    socket.emit("message", {
      data: JSON.stringify({
        state: "starting",
        remaining: 1790,
        siteUrl: "https://x.test",
        extra: 1,
      }),
    });
    socket.emit("message", { data: "not json" });
    socket.emit("message", { data: JSON.stringify({ state: "bogus", remaining: 1 }) });
    socket.emit("message", {
      data: JSON.stringify({ state: "ready", remaining: "lots", siteUrl: "javascript:alert(1)" }),
    });
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

  it("gives up on a stalled handshake and polls", async () => {
    const page = loadPage({ state: "ready", remaining: 800, siteUrl: "" });
    // The socket never opens or closes on its own.
    await page.runTimers();
    assert.equal(FakeSocket.opened[0]!.readyState, 3);
    await page.runTimers();
    assert.equal(page.polls, 1);
    assert.deepEqual(page.statuses[0], { state: "ready", remaining: 800, siteUrl: "" });
    assert.equal(FakeSocket.opened.length, 2);
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
