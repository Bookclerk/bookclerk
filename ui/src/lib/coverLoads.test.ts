import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/** The observer coverLoads creates, driven by the test. */
let fire: (target: Element, isIntersecting: boolean) => void = () => {};

class FakeObserver {
  constructor(callback: (entries: Array<{ target: Element; isIntersecting: boolean }>) => void) {
    fire = (target, isIntersecting) => callback([{ target, isIntersecting }]);
  }
  observe() {}
  unobserve() {}
  disconnect() {}
}

beforeEach(() => {
  vi.resetModules();
  vi.stubGlobal("IntersectionObserver", FakeObserver);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

const el = () => ({}) as Element;

describe("coversSettled", () => {
  it("resolves at once when no cover on screen is loading", async () => {
    const { coversSettled, trackCoverLoad } = await import("./coverLoads");
    // Loading but off screen (a lazy image, or clipped by a shelf).
    const a = el();
    trackCoverLoad(a);
    fire(a, false);
    await expect(coversSettled(1_000)).resolves.toBeUndefined();
  });

  it("waits for the covers on screen to load", async () => {
    const { coversLoadingOnScreen, coversSettled, trackCoverLoad } = await import("./coverLoads");
    const a = el();
    const b = el();
    const doneA = trackCoverLoad(a);
    const doneB = trackCoverLoad(b);
    fire(a, true);
    fire(b, true);
    expect(coversLoadingOnScreen()).toBe(2);
    let settled = false;
    const wait = coversSettled(60_000).then(() => (settled = true));
    doneA();
    await Promise.resolve();
    expect(settled).toBe(false);
    // Scrolling away counts as much as loading.
    fire(b, false);
    await wait;
    expect(settled).toBe(true);
    doneB();
    expect(coversLoadingOnScreen()).toBe(0);
  });

  it("waits for a new cover's first report before deciding", async () => {
    const { coversSettled, trackCoverLoad } = await import("./coverLoads");
    const a = el();
    trackCoverLoad(a);
    let settled = false;
    const wait = coversSettled(60_000).then(() => (settled = true));
    await Promise.resolve();
    expect(settled).toBe(false);
    // Reported off screen: nothing to wait for.
    fire(a, false);
    await wait;
    expect(settled).toBe(true);
  });

  it("gives up waiting after the cap", async () => {
    vi.useFakeTimers();
    const { coversSettled, trackCoverLoad } = await import("./coverLoads");
    const a = el();
    trackCoverLoad(a);
    fire(a, true);
    const wait = coversSettled(2_000);
    await vi.advanceTimersByTimeAsync(2_000);
    await expect(wait).resolves.toBeUndefined();
  });
});
