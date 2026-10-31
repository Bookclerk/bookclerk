// Exercise the shipped optional interaction with deterministic DOM geometry.
// An independent center/extent oracle verifies the visible sheet bounds.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { URL } from "node:url";
import { test } from "vitest";
import {
  catalogCards,
  catalogPool,
  catalogRecords,
  catalogGroups,
  catalogSource,
} from "./catalog.ts";

/** Minimal element identity for card-only hit testing. */
class TestElement {
  /** Whether this target is an actual catalog sheet. */
  card: boolean;
  /** Creates either a sheet or unrelated page content. */
  constructor(card = false) {
    this.card = card;
  }
  /** Resolves only sheet hit tests; the surrounding whitespace is not interactive. */
  closest(selector: string) {
    return selector === ".catalog-card" && this.card ? this : null;
  }
}

/** Browser fields consumed by wheel, pointer and lifecycle handlers. */
interface Gesture {
  /** Vertical wheel travel in the current delta mode. */
  deltaY: number;
  /** Horizontal wheel travel. */
  deltaX: number;
  /** Zoom modifier. */
  ctrlKey: boolean;
  /** Sideways-scroll modifier. */
  shiftKey: boolean;
  /** Whether native default behavior can be canceled. */
  cancelable: boolean;
  /** Stable pointer identity throughout a drag. */
  pointerId: number;
  /** Mouse, touch or pen input type. */
  pointerType: string;
  /** Whether this is the primary contact. */
  isPrimary: boolean;
  /** Pressed pointer button; zero is primary. */
  button: number;
  /** Pointer position in viewport CSS pixels. */
  clientX: number;
  /** Pointer position in viewport CSS pixels. */
  clientY: number;
  /** CSS animation completion name. */
  animationName: string;
  /** Native event target, optionally a sheet or stamp. */
  target: TestElement;
  /** Whether the controller canceled this event. */
  prevented: boolean;
  /** Records cancellation without moving the simulated document. */
  preventDefault(): void;
}

/** Viewport and document dimensions for a responsive fixture. */
interface Device {
  /** Viewport width in CSS pixels. */
  width?: number;
  /** Viewport height in CSS pixels. */
  height?: number;
  /** Artwork's document top. */
  stageTop?: number;
  /** Desktop artwork width, before sheet scaling. */
  stageWidth?: number;
  /** Artwork's left edge in the document. */
  stageLeft?: number;
  /** Sticky header height. */
  headerHeight?: number;
  /** Notice height before it scrolls away. */
  noticeHeight?: number;
  /** Initial motion preference. */
  reduced?: boolean;
  /** Whether keyframe animations are supported. */
  animations?: boolean;
}

/** Runs the actual controller against a small DOM and native-event simulation. */
function controller(device: Device = {}) {
  /** Event handlers on the window, stack and decorative stage. */
  const listeners = new Map<string, (event: Gesture) => void>();
  const stackListeners = new Map<string, (event: Gesture) => void>();
  const stageListeners = new Map<string, (event: Gesture) => void>();
  /** Completion deadlines and pending pre-paint work. */
  const timers = new Map<number, { at: number; callback: () => void }>();
  const frames = new Map<number, () => void>();
  /** Fitted stack properties and simulated monotonic time. */
  const properties = new Map<string, string>();
  let now = 10;
  let nextId = 0;
  let y = 0;
  /** Layout and viewport measurements, in CSS pixels. */
  const width = device.width ?? 1280;
  let viewHeight = device.height ?? 800;
  const stageWidth = device.stageWidth ?? (width <= 900 ? width - 55 : 460);
  const stageLeft = device.stageLeft ?? (width <= 900 ? 20 : 680);
  const stageTop = device.stageTop ?? 169;
  const headerHeight = device.headerHeight ?? 81;
  const noticeHeight = device.noticeHeight ?? 43;
  /** Four complete sheets with their own CSS properties and pointer ownership. */
  const sheets = Array.from({ length: 4 }, (_, id) => {
    const styleValues = new Map<string, string>();
    const classes = new Set<string>();
    const pointers = new Set<number>();
    const stamp = new TestElement();
    return Object.assign(new TestElement(true), {
      id,
      stamp,
      classes,
      pointers,
      styleValues,
      querySelector: () => stamp,
      style: {
        setProperty: (key: string, value: string) => styleValues.set(key, value),
        getPropertyValue: (key: string) => styleValues.get(key) ?? "",
        removeProperty: (key: string) => styleValues.delete(key),
      },
      classList: {
        add: (...names: string[]) => names.forEach((name) => classes.add(name)),
        remove: (...names: string[]) => names.forEach((name) => classes.delete(name)),
      },
      setPointerCapture: (id: number) => pointers.add(id),
      hasPointerCapture: (id: number) => pointers.has(id),
      releasePointerCapture: (id: number) => pointers.delete(id),
    });
  });
  /** Fixed-proportion layout canvas and card-local event surface. */
  const stack = {
    offsetWidth: 446,
    offsetHeight: 380,
    offsetTop: 24,
    offsetLeft: 0,
    querySelectorAll: () => sheets,
    append: (sheet: (typeof sheets)[number]) => {
      sheets.splice(sheets.indexOf(sheet), 1);
      sheets.push(sheet);
    },
    style: { setProperty: (key: string, value: string) => properties.set(key, value) },
    addEventListener: (name: string, callback: (event: Gesture) => void) =>
      stackListeners.set(name, callback),
    removeEventListener: (name: string) => stackListeners.delete(name),
  };
  /** Responsive artwork area, whose height follows its fitted stack on mobile. */
  const stage = {
    dataset: {} as { catalogState?: string },
    style: { setProperty: (key: string, value: string) => properties.set(key, value) },
    querySelector: () => stack,
    getBoundingClientRect: () => ({
      top: stageTop - y,
      left: stageLeft,
      width: stageWidth,
      height: width <= 900 ? parseFloat(properties.get("--catalog-height") ?? "420") : 460,
    }),
    addEventListener: (name: string, callback: (event: Gesture) => void) =>
      stageListeners.set(name, callback),
  };
  /** Occluding surfaces and live motion preference. */
  const header = { getBoundingClientRect: () => ({ height: headerHeight }) };
  const notice = { getBoundingClientRect: () => ({ height: noticeHeight }) };
  let preferenceChanged = () => {};
  const preference = {
    matches: device.reduced ?? false,
    addEventListener: (_: string, callback: () => void) => {
      preferenceChanged = callback;
    },
  };
  /** Browser primitives; any page-repositioning attempt fails the test. */
  const browser = {
    get scrollY() {
      return y;
    },
    innerWidth: width,
    get innerHeight() {
      return viewHeight;
    },
    scrollTo: () => assert.fail("The optional easter egg must never reposition the page"),
    setTimeout: (callback: () => void, delay: number) => {
      timers.set(++nextId, { callback, at: now + delay });
      return nextId;
    },
    clearTimeout: (id: number) => timers.delete(id),
    requestAnimationFrame: (callback: () => void) => {
      frames.set(++nextId, callback);
      return nextId;
    },
    cancelAnimationFrame: (id: number) => frames.delete(id),
    addEventListener: (name: string, callback: (event: Gesture) => void) =>
      listeners.set(name, callback),
    removeEventListener: (name: string) => listeners.delete(name),
  };
  runInNewContext(readFileSync(new URL("src/catalog.js", import.meta.url), "utf8"), {
    document: {
      querySelector: (selector: string) =>
        selector === "[data-catalog]" ? stage : selector === ".site-header" ? header : notice,
      documentElement: { clientWidth: width - 15 },
    },
    CSS: { supports: () => device.animations ?? true },
    Element: TestElement,
    getComputedStyle: (element: object) => ({
      top: "0px",
      getPropertyValue: (name: string) => {
        const index = Math.max(
          0,
          sheets.findIndex((s) => s === element),
        );
        return String(
          name === "--card-x" ? index * 13 : name === "--card-y" ? index * 14 : index * 2.4,
        );
      },
    }),
    matchMedia: () => preference,
    performance: { now: () => now },
    window: browser,
  });
  /** Dispatches real handler input on its owning event surface. */
  function gesture(
    type: string,
    fields: Partial<Gesture> = {},
    scope: "stack" | "window" | "stage" = "stack",
  ) {
    const event: Gesture = {
      deltaY: 50,
      deltaX: 0,
      ctrlKey: false,
      shiftKey: false,
      cancelable: true,
      pointerId: 1,
      pointerType: "touch",
      isPrimary: true,
      button: 0,
      clientX: 100,
      clientY: 200,
      animationName: "",
      target: sheets[0] ?? new TestElement(),
      prevented: false,
      preventDefault() {
        this.prevented = true;
      },
      ...fields,
    };
    (scope === "stack" ? stackListeners : scope === "window" ? listeners : stageListeners).get(
      type,
    )?.(event);
    return event;
  }
  /** Advances deadlines in chronological order, including callbacks scheduled by timers. */
  function tick(milliseconds: number) {
    const end = now + milliseconds;
    for (;;) {
      const next = [...timers]
        .filter(([, timer]) => timer.at <= end)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      timers.delete(next[0]);
      now = next[1].at;
      next[1].callback();
    }
    now = end;
  }
  /** Independent rotated-center and half-extent oracle for the stack and wheel lift. */
  function fullyVisible(peak = false) {
    const scale = Number(properties.get("--catalog-scale") ?? 1);
    const originX = stageLeft + parseFloat(properties.get("--catalog-offset-x") ?? "0");
    const originY = stageTop - y + 24 + parseFloat(properties.get("--catalog-offset-y") ?? "0");
    const poses = sheets.map((_, index) => ({
      x: index * 13,
      y: index * 14,
      angle: index * 2.4,
      scale: 1,
    }));
    if (peak)
      poses.push({
        x: parseFloat(properties.get("--catalog-side") ?? "46"),
        y: -parseFloat(properties.get("--catalog-lift") ?? "95"),
        angle: parseFloat(properties.get("--catalog-turn") ?? "9"),
        scale: Number(properties.get("--catalog-lift-scale") ?? 1.035),
      });
    return poses.every((p) => {
      const a = (p.angle * Math.PI) / 180,
        c = Math.cos(a),
        s = Math.sin(a);
      const cx = 223 + p.x + 76 * s * p.scale,
        cy = 266 + p.y - 76 * c * p.scale;
      const hw = ((Math.abs(c) * 446 + Math.abs(s) * 380) * p.scale) / 2;
      const hh = ((Math.abs(s) * 446 + Math.abs(c) * 380) * p.scale) / 2;
      return (
        originX + (cx - hw) * scale >= 15 &&
        originX + (cx + hw) * scale <= width - 30 &&
        originY + (cy - hh) * scale >= headerHeight + Math.max(0, noticeHeight - y) + 15 &&
        originY + (cy + hh) * scale <= viewHeight - 15
      );
    });
  }
  return {
    state: () => stage.dataset.catalogState,
    y: () => y,
    sheets,
    properties,
    listeners,
    stackListeners,
    gesture,
    tick,
    fullyVisible,
    order: () => sheets.map((s) => s.id),
    scroll: (position: number) => {
      y = position;
      gesture("scroll", {}, "window");
      for (const [id, callback] of [...frames]) {
        frames.delete(id);
        callback();
      }
    },
    resize: (height: number) => {
      viewHeight = height;
      gesture("resize", {}, "window");
    },
    reduce: () => {
      preference.matches = true;
      preferenceChanged();
    },
  };
}

test("every catalog sheet is complete before JavaScript runs", () => {
  const cards = catalogCards();
  assert.equal(cards.match(/class="catalog-card"/g)?.length, 4);
  assert.ok(cards.includes("<strong>Dungeon Crawler Carl</strong>"));
  assert.equal(cards.match(/<dt>Series<\/dt>/g)?.length, 4);
  assert.equal(cards.match(/<dt>Shelved<\/dt>/g)?.length, 4);
  assert.ok(!cards.includes('class="back"'));
});

test("record text cannot introduce markup or attributes", () => {
  const record = {
    title: '<img src=x onerror="alert(1)">',
    number: "0001",
    author: "A & B",
    narrators: ['A "reader"'],
    durationMinutes: 60,
    series: { name: "<Series>", number: 1 },
    sourceUrl: "https://example.com/edition",
    verifiedOn: "2026-10-31",
    bought: "A store",
    shelved: "/audiobooks/<title>",
  };
  const cards = catalogCards([record, record]);
  assert.ok(!cards.includes("<img"));
  assert.ok(cards.includes("&lt;img src=x onerror=&quot;alert(1)&quot;&gt;"));
  assert.ok(cards.includes("A &amp; B"));
  assert.ok(cards.includes("&lt;Series&gt; #1"));
  assert.ok(cards.includes('title="A &quot;reader&quot;"'));
});

test("the drop never captures page scrolling or creates global wheel handlers", () => {
  const page = controller();
  assert.equal(page.state(), "entering");
  assert.equal(page.listeners.has("wheel"), false);
  assert.equal(page.listeners.has("touchmove"), false);
  assert.equal(page.gesture("wheel", {}, "window").prevented, false);
  assert.equal(page.gesture("wheel").prevented, false);
  page.tick(1550);
  assert.equal(page.state(), "ready");
  page.scroll(500);
  assert.equal(page.y(), 500);
  assert.deepEqual(page.order(), [0, 1, 2, 3]);
});

test("a stack reached later drops once and then supports the same optional interaction", () => {
  const page = controller({
    width: 390,
    height: 844,
    stageTop: 1000,
    headerHeight: 122,
    noticeHeight: 35,
  });
  assert.equal(page.state(), "waiting");
  page.scroll(400);
  assert.equal(page.state(), "waiting");
  page.scroll(800);
  assert.equal(page.state(), "entering");
  page.tick(1550);
  assert.equal(page.state(), "ready");
  assert.equal(page.gesture("wheel").prevented, true);
  assert.equal(page.y(), 800);
  page.tick(1250);
  assert.deepEqual(page.order(), [1, 2, 3, 0]);
});

test("only wheel gestures over an actual sheet shuffle; whitespace stays native", () => {
  const page = controller();
  page.tick(1550);
  assert.equal(page.gesture("wheel", { target: new TestElement() }).prevented, false);
  assert.equal(page.gesture("wheel", {}, "window").prevented, false);
  page.tick(301);
  assert.equal(page.gesture("wheel").prevented, true);
  assert.equal(page.state(), "shuffling");
  assert.equal(page.fullyVisible(true), true);
  page.tick(1250);
  assert.equal(page.state(), "ready");
  assert.deepEqual(page.order(), [1, 2, 3, 0]);
});

test("repeated independent wheel gestures cycle all records and return to the start", () => {
  const page = controller();
  page.tick(1550);
  for (let i = 1; i <= 8; i += 1) {
    assert.equal(page.gesture("wheel", { deltaY: i % 2 ? 100 : -100 }).prevented, true);
    page.tick(1250);
    assert.equal(page.order()[0], i % 4);
    assert.equal(new Set(page.order()).size, 4);
    assert.equal(page.y(), 0);
    assert.equal(page.state(), "ready");
  }
});

test("one wheel burst cannot trigger another turn through residual momentum", () => {
  const page = controller();
  page.tick(1550);
  page.gesture("wheel");
  page.tick(1200);
  assert.equal(page.gesture("wheel").prevented, true);
  page.tick(50);
  assert.equal(page.state(), "ready");
  assert.equal(page.gesture("wheel").prevented, false);
  assert.deepEqual(page.order(), [1, 2, 3, 0]);
  page.tick(301);
  assert.equal(page.gesture("wheel").prevented, true);
});

test("zoom, sideways and noncancelable wheel input remain native", () => {
  for (const fields of [
    { ctrlKey: true },
    { shiftKey: true },
    { deltaX: 100 },
    { cancelable: false },
    { deltaY: 0 },
  ]) {
    const page = controller();
    page.tick(1550);
    assert.equal(page.gesture("wheel", fields).prevented, false);
    assert.equal(page.state(), "ready");
  }
});

test("a touch drag follows the pointer and release sends the front record to the back", () => {
  for (const direction of [-1, 1]) {
    const page = controller();
    page.tick(1550);
    const front = page.sheets[0];
    assert.ok(front);
    page.gesture("pointerdown");
    assert.equal(front.pointers.has(1), true);
    page.gesture("pointermove", { clientX: 100 + direction * 80, clientY: 180 });
    assert.equal(page.state(), "dragging");
    assert.ok(direction * parseFloat(front.styleValues.get("--drag-x") ?? "0") > 20);
    page.gesture("pointerup");
    assert.equal(front.pointers.size, 0);
    assert.equal(page.state(), "shuffling");
    assert.ok(front.classes.has("is-outgoing"));
    page.tick(1250);
    assert.deepEqual(page.order(), [1, 2, 3, 0]);
    assert.equal(front.styleValues.has("--drag-x"), false);
    assert.equal(page.y(), 0);
  }
});

test("a tap and a vertical touch swipe do not shuffle or cancel page scrolling", () => {
  const page = controller();
  page.tick(1550);
  page.gesture("pointerdown");
  page.gesture("pointerup");
  assert.equal(page.state(), "ready");
  page.gesture("pointerdown");
  assert.equal(page.gesture("pointermove", { clientY: 120 }).prevented, false);
  page.gesture("pointercancel");
  assert.equal(page.state(), "ready");
  assert.deepEqual(page.order(), [0, 1, 2, 3]);
  assert.equal(page.sheets[0]?.pointers.size, 0);
});

test("backing cards, secondary buttons and multi-touch cannot start a drag", () => {
  const page = controller();
  page.tick(1550);
  const backing = page.sheets[1];
  assert.ok(backing);
  page.gesture("pointerdown", { target: backing });
  page.gesture("pointermove", { clientX: 200 });
  assert.equal(page.state(), "ready");
  page.gesture("pointerdown", { button: 2 });
  page.gesture("pointermove", { clientX: 200 });
  assert.equal(page.state(), "ready");
  page.gesture("pointerdown");
  page.gesture("pointermove", { clientX: 180 });
  page.gesture("pointerdown", { isPrimary: false, pointerId: 2 });
  assert.equal(page.state(), "ready");
  assert.equal(page.sheets[0]?.pointers.size, 0);
  assert.deepEqual(page.order(), [0, 1, 2, 3]);
});

test("pointer cancellation, lost capture, keys, resize and navigation release a dragged card", () => {
  for (const exit of ["pointercancel", "lostpointercapture", "keydown", "resize", "pagehide"]) {
    const page = controller();
    page.tick(1550);
    page.gesture("pointerdown", { pointerType: "mouse" });
    page.gesture("pointermove", { pointerType: "mouse", clientX: 160, clientY: 170 });
    assert.equal(page.state(), "dragging");
    if (exit === "resize") page.resize(780);
    else page.gesture(exit, {}, exit === "keydown" || exit === "pagehide" ? "window" : "stack");
    assert.equal(page.state(), "ready");
    assert.equal(page.sheets[0]?.pointers.size, 0);
    assert.deepEqual(page.order(), [0, 1, 2, 3]);
  }
});

test("an old entrance stamp cannot finish a new shuffle; its incoming stamp can", () => {
  const page = controller();
  page.tick(1550);
  const oldStamp = page.sheets[0]?.stamp;
  const newStamp = page.sheets[1]?.stamp;
  assert.ok(oldStamp);
  assert.ok(newStamp);
  page.gesture("wheel");
  page.gesture("animationend", { animationName: "catalog-stamp", target: oldStamp }, "stage");
  assert.equal(page.state(), "shuffling");
  page.gesture("animationend", { animationName: "catalog-stamp", target: newStamp }, "stage");
  assert.equal(page.state(), "ready");
  assert.deepEqual(page.order(), [1, 2, 3, 0]);
  page.tick(1250);
  assert.deepEqual(page.order(), [1, 2, 3, 0]);
});

test("reduced motion cancels interaction and releases active pointer ownership", () => {
  const page = controller();
  page.tick(1550);
  page.gesture("pointerdown");
  page.gesture("pointermove", { clientX: 180 });
  page.reduce();
  assert.equal(page.state(), "static");
  assert.equal(page.sheets[0]?.pointers.size, 0);
  assert.equal(page.gesture("wheel").prevented, false);
  assert.equal(page.stackListeners.has("pointerdown"), false);
  const reduced = controller({ reduced: true });
  assert.equal(reduced.state(), "static");
  assert.equal(reduced.stackListeners.size, 0);
});

test("short landscape views keep centered static artwork and native scrolling", () => {
  const page = controller({ width: 844, height: 390, stageTop: 600 });
  assert.equal(page.state(), "static");
  assert.equal(page.gesture("wheel").prevented, false);
  assert.ok(parseFloat(page.properties.get("--catalog-offset-x") ?? "0") > 100);
  assert.ok(parseFloat(page.properties.get("--catalog-height") ?? "0") < 320);
});

test("optional wheel lifts stay inside complete phone, Duo and landscape views", () => {
  for (const device of [
    { width: 390, height: 844, stageTop: 180, headerHeight: 122, noticeHeight: 35 },
    { width: 540, height: 720, stageTop: 180, headerHeight: 122, noticeHeight: 35 },
    { width: 720, height: 540, stageTop: 146, headerHeight: 122, noticeHeight: 0 },
    { width: 320, height: 568, stageTop: 180, headerHeight: 122, noticeHeight: 35 },
  ]) {
    const page = controller(device);
    page.tick(1550);
    assert.equal(page.state(), "ready", JSON.stringify(device));
    assert.equal(page.gesture("wheel").prevented, true);
    assert.equal(page.fullyVisible(true), true);
  }
});

test("unsupported animations leave the server-rendered artwork alone", () => {
  const page = controller({ animations: false });
  assert.equal(page.state(), undefined);
  assert.equal(page.stackListeners.size, 0);
  assert.equal(page.gesture("wheel").prevented, false);
});

test("the authored pool includes every main novel in the requested series", () => {
  for (const [name, expected] of [
    ["Dungeon Crawler Carl", 8],
    ["The Expanse", 9],
    ["Dune", 6],
    ["Sprawl", 3],
    ["Mistborn", 7],
    ["Stormlight Archive", 5],
    ["Foundation", 7],
    ["The Wheel of Time", 15],
  ] as const) {
    const selected = catalogRecords.filter((r) => r.series?.name === name);
    assert.ok(selected.length >= expected, name);
    const positions = new Set(selected.map((r) => r.series?.number));
    for (
      let i = name === "The Wheel of Time" ? 0 : 1;
      i < (name === "The Wheel of Time" ? 15 : expected + 1);
      i += 1
    )
      assert.ok(positions.has(i), `${name} #${i}`);
  }
  for (const title of [
    "Elantris",
    "Warbreaker",
    "Tress of the Emerald Sea",
    "Yumi and the Nightmare Painter",
    "The Sunlit Man",
    "Isles of the Emberdark",
    "The Martian",
    "Snow Crash",
    "Fahrenheit 451",
  ])
    assert.ok(
      catalogRecords.some((r) => r.title === title),
      title,
    );
  assert.ok(catalogRecords.length >= 69);
  assert.equal(catalogPool().match(/class="catalog-card"/g)?.length, catalogRecords.length);
});

test("every edition has complete, uniquely numbered and traceable metadata", () => {
  assert.equal(new Set(catalogRecords.map((r) => r.number)).size, catalogRecords.length);
  assert.equal(new Set(catalogRecords.map((r) => r.title)).size, catalogRecords.length);
  for (const record of catalogRecords) {
    assert.ok(record.author.trim());
    assert.ok(record.narrators.length > 0 && record.narrators.every((n) => n.trim()));
    assert.ok(Number.isInteger(record.durationMinutes) && record.durationMinutes > 0);
    assert.equal(new URL(record.sourceUrl).protocol, "https:");
    assert.match(record.verifiedOn, /^\d{4}-\d{2}-\d{2}$/);
    if (record.series)
      assert.ok(Number.isInteger(record.series.number) && record.series.number >= 0);
  }
});

test("large casts retain full credit without crowding the narrator row", () => {
  const dune = catalogRecords.find((r) => r.title === "Dune");
  assert.ok(dune);
  const rendered = catalogCards([dune, dune]);
  assert.ok(rendered.includes("Scott Brick + 11"));
  assert.ok(rendered.includes("Simon Vance"));
  assert.ok(rendered.includes("21 h 2 min"));
});

/** Runs the shipped sampler against deliberately unequal series sizes. */
function sampled(random: () => number, firstGroupSize = 30) {
  const originals = [firstGroupSize, 1, 2, 3, 1, 1].map((size, group) =>
    Array.from({ length: size }, (_, index) => ({
      id: group * 1000 + index,
      group,
      cloneNode: () => ({ id: group * 1000 + index, group }),
    })),
  );
  let chosen: { id: number; group: number }[] = [];
  const stack = {
    replaceChildren: (...cards: { id: number; group: number }[]) => {
      chosen = cards;
    },
    querySelectorAll: () => chosen,
  };
  const groups = originals.map((books) => ({ querySelectorAll: () => books }));
  const stage = {
    querySelector: (selector: string) =>
      selector === ".catalog-stack" ? stack : { content: { querySelectorAll: () => groups } },
  };
  const math = Object.create(Math) as Math;
  math.random = random;
  runInNewContext(readFileSync(new URL("src/catalog.js", import.meta.url), "utf8"), {
    document: { querySelector: () => stage },
    Math: math,
    matchMedia: () => ({ matches: true }),
    CSS: { supports: () => false },
  });
  return { chosen, originals: originals.map((books) => books.map((book) => book.id)) };
}

test("each page samples four distinct groups, then one cloned book from each", () => {
  const low = sampled(() => 0);
  const high = sampled(() => 0.999999);
  for (const sample of [low, high]) {
    assert.equal(sample.chosen.length, 4);
    assert.equal(new Set(sample.chosen.map((book) => book.group)).size, 4);
    assert.equal(new Set(sample.chosen.map((book) => book.id)).size, 4);
    assert.deepEqual(
      sample.originals[0],
      Array.from({ length: 30 }, (_, index) => index),
    );
  }
  assert.notDeepEqual(low.chosen, high.chosen);
  assert.ok(high.chosen.some((book) => book.group === 0 && book.id === 29));
});

test("adding books to a large series never changes its group-selection probability", () => {
  /** Replays the same random stream for both catalog sizes. */
  const seed = () => {
    let state = 173;
    return () => {
      state = (state * 16807) % 2147483647;
      return state / 2147483647;
    };
  };
  const small = sampled(seed(), 1);
  const large = sampled(seed(), 300);
  assert.deepEqual(
    small.chosen.map((book) => book.group),
    large.chosen.map((book) => book.group),
  );
});

test("the grouped JSON has shared series context and exactly one sampling owner per book", () => {
  assert.ok(catalogSource.series.length >= 9);
  assert.ok(catalogSource.standalones.length >= 8);
  const grouped = catalogGroups.flatMap((group) => group.books);
  assert.equal(grouped.length, catalogRecords.length);
  assert.equal(new Set(grouped.map((book) => book.number)).size, catalogRecords.length);
  const cosmere = catalogGroups.find((group) => group.key === "universe:Cosmere");
  assert.ok(cosmere);
  assert.equal(
    cosmere.books.length,
    catalogRecords.filter((book) => book.universe === "Cosmere").length,
  );
  for (const title of ["The Martian", "Snow Crash", "Fahrenheit 451"]) {
    const group = catalogGroups.find((group) => group.name === title);
    assert.ok(group);
    assert.equal(group.books.length, 1);
  }
  for (const series of catalogSource.series) {
    assert.ok(series.books.length > 0);
    for (const book of series.books) {
      assert.equal("series" in book, false);
      assert.equal("universe" in book, false);
      assert.ok(Number.isInteger(book.seriesNumber));
    }
  }
});

test("the no-script fallback also shows different sampling groups", () => {
  const groups = [...catalogCards().matchAll(/data-group="([^"]+)"/g)].map((match) => match[1]);
  assert.equal(groups.length, 4);
  assert.equal(new Set(groups).size, 4);
});
