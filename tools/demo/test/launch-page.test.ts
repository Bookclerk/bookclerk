// The launch page's Turnstile renderer (public/_demo/launch.js): the size it
// picks, and how it brings the check back on a page restored from the
// back/forward cache or shown again after a while in the background. It is
// a browser script, so it runs in a node:vm context with stand-ins for the
// page and for Turnstile, outside the Workers runtime.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { URL } from "node:url";
import { describe, it } from "vitest";

/** A `.cf-turnstile` container stand-in. */
type Container = {
  /** The container's data attributes. */
  dataset: { sitekey: string; action: string };
  /** Rendered height in pixels: the stylesheet reserves it, widget or not. */
  offsetHeight: number;
  /** What Turnstile put inside: its frame's wrapper, 0 high once the frame is gone. */
  children: Array<{ offsetHeight: number }>;
  /** How many times the page emptied the container. */
  cleared: number;
  /** Empties the container. */
  replaceChildren: () => void;
};

/** What a page running launch.js did, and handles to drive it. */
type Page = {
  /** The page's one container. */
  el: Container;
  /** Each render call's options and widget id, in order. */
  renders: Array<{ sitekey: string; action: string; size: string; id: string }>;
  /** Widget ids the page removed. */
  removed: string[];
  /** Widget ids Turnstile reports expired. */
  expired: Set<string>;
  /** Times the page asked to reload. */
  reloads: number;
  /** The fake clock, in milliseconds. */
  now: number;
  /** Whether the narrow-screen media query matches. */
  narrow: boolean;
  /** Turnstile's api.js arrives and calls the onload callback. */
  loadTurnstile: () => void;
  /** Turnstile's script goes away (a restored page without it). */
  dropTurnstile: () => void;
  /** The tab goes to the background. */
  hide: () => void;
  /** The tab is shown again. */
  show: () => void;
  /** pageshow fires; `persisted` means restored from the back/forward cache. */
  pageshow: (persisted: boolean) => void;
  /**
   * A back/forward cache restore: visibilitychange then persisted pageshow
   * in one task, as the HTML spec orders them, or the reverse.
   */
  restore: (order?: "spec" | "reversed") => void;
};

/**
 * Loads launch.js into a fake page with one Turnstile container.
 *
 * @returns The page.
 */
function loadPage(): Page {
  const el: Container = {
    dataset: { sitekey: "1x00000000000000000000AA", action: "demo_start" },
    offsetHeight: 65,
    children: [],
    cleared: 0,
    replaceChildren: () => {
      el.cleared += 1;
      el.children = [];
    },
  };
  const winListeners = new Map<string, Array<(e: { persisted: boolean }) => void>>();
  const docListeners: Array<() => void> = [];
  let timers: Array<() => void> = [];
  /** Runs the timers the page set, as the event loop would after the task. */
  const flush = () => {
    const due = timers;
    timers = [];
    for (const fn of due) fn();
  };
  const visible = () => {
    document.visibilityState = "visible";
    docListeners.forEach((fn) => fn());
  };
  const persisted = (p: boolean) => {
    for (const fn of winListeners.get("pageshow") ?? []) fn({ persisted: p });
  };
  const document = {
    visibilityState: "visible",
    querySelectorAll: () => [el],
    addEventListener: (_type: string, fn: () => void) => docListeners.push(fn),
  };
  const turnstile = {
    render: (target: Container, opts: { sitekey: string; action: string; size: string }) => {
      const id = `w${page.renders.length}`;
      page.renders.push({ sitekey: opts.sitekey, action: opts.action, size: opts.size, id });
      target.children = [{ offsetHeight: 65 }];
      return id;
    },
    remove: (id: string) => page.removed.push(id),
    isExpired: (id: string) => page.expired.has(id),
  };
  const window: Record<string, unknown> = {
    matchMedia: () => ({ matches: page.narrow }),
    location: { reload: () => (page.reloads += 1) },
    addEventListener: (type: string, fn: (e: { persisted: boolean }) => void) =>
      winListeners.set(type, [...(winListeners.get(type) ?? []), fn]),
  };
  const page: Page = {
    el,
    renders: [],
    removed: [],
    expired: new Set(),
    reloads: 0,
    now: 1_000_000,
    narrow: false,
    loadTurnstile: () => {
      window.turnstile = turnstile;
      (window.bookclerkDemoTurnstile as () => void)();
    },
    dropTurnstile: () => {
      delete window.turnstile;
    },
    hide: () => {
      document.visibilityState = "hidden";
      docListeners.forEach((fn) => fn());
    },
    show: () => {
      visible();
      flush();
    },
    pageshow: (p) => {
      persisted(p);
      flush();
    },
    restore: (order = "spec") => {
      if (order === "spec") {
        visible();
        persisted(true);
      } else {
        persisted(true);
        visible();
      }
      flush();
    },
  };
  runInNewContext(readFileSync(new URL("../public/_demo/launch.js", import.meta.url), "utf8"), {
    window,
    document,
    Date: { now: () => page.now },
    Map,
    Array,
    setTimeout: (fn: () => void) => timers.push(fn),
  });
  return page;
}

describe("launch.js", () => {
  it("renders each container once Turnstile loads, compact on narrow screens", () => {
    const wide = loadPage();
    wide.loadTurnstile();
    const key = { sitekey: "1x00000000000000000000AA", action: "demo_start" };
    assert.deepEqual(wide.renders, [{ ...key, size: "normal", id: "w0" }]);

    const narrow = loadPage();
    narrow.narrow = true;
    narrow.loadTurnstile();
    assert.deepEqual(narrow.renders, [{ ...key, size: "compact", id: "w0" }]);
  });

  it("renders the check afresh on a page restored from the back/forward cache", () => {
    const page = loadPage();
    page.loadTurnstile();
    page.pageshow(false);
    assert.equal(page.renders.length, 1);
    page.pageshow(true);
    assert.equal(page.renders.length, 2);
    assert.deepEqual(page.removed, ["w0"]);
    assert.equal(page.el.cleared, 2);
  });

  it("reloads a restored page whose Turnstile script is gone", () => {
    const page = loadPage();
    page.pageshow(true);
    assert.equal(page.reloads, 1);
    assert.equal(page.renders.length, 0);
  });

  it("renders afresh after a long time hidden, but keeps a good widget after a short one", () => {
    const page = loadPage();
    page.loadTurnstile();
    page.hide();
    page.now += 60_000;
    page.show();
    assert.equal(page.renders.length, 1);
    page.hide();
    page.now += 4 * 60_000;
    page.show();
    assert.equal(page.renders.length, 2);
    assert.deepEqual(page.removed, ["w0"]);
  });

  it("renders again when a short absence left the widget collapsed or expired", () => {
    const page = loadPage();
    page.loadTurnstile();
    page.hide();
    page.el.children[0]!.offsetHeight = 0;
    page.show();
    assert.equal(page.renders.length, 2);
    page.hide();
    page.expired.add("w1");
    page.show();
    assert.equal(page.renders.length, 3);
    assert.deepEqual(page.removed, ["w0", "w1"]);
  });

  it("waits for Turnstile on a fresh page shown again before it loaded", () => {
    const page = loadPage();
    page.hide();
    page.show();
    assert.equal(page.reloads, 0);
    page.hide();
    page.now += 5 * 60_000;
    page.show();
    assert.equal(page.reloads, 1);
  });

  it("renders once when a restore fires visibilitychange and pageshow, in either order", () => {
    for (const order of ["spec", "reversed"] as const) {
      const page = loadPage();
      page.loadTurnstile();
      page.hide();
      page.now += 10 * 60_000;
      page.restore(order);
      assert.equal(page.renders.length, 2, order);
      assert.deepEqual(page.removed, ["w0"], order);
    }
  });

  it("keeps a healthy widget in its reserved space after a short absence", () => {
    const page = loadPage();
    page.loadTurnstile();
    page.hide();
    page.show();
    assert.equal(page.renders.length, 1);
    assert.equal(page.el.cleared, 1);
  });
});
