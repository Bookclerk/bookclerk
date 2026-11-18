import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/**
 * A `sessionStorage` stand-in backed by a map.
 *
 * @returns The stand-in and its map.
 */
function memoryStorage() {
  const data = new Map<string, string>();
  return {
    data,
    getItem: (k: string) => data.get(k) ?? null,
    setItem: (k: string, v: string) => void data.set(k, v),
    removeItem: (k: string) => void data.delete(k),
  };
}

let storage = memoryStorage();

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  storage = memoryStorage();
  vi.stubGlobal("sessionStorage", storage);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("createLookupCache", () => {
  it("keeps answers per title for the tab, across a reload", async () => {
    const first = await import("./lookupCache");
    const cache = first.createLookupCache<number>("t", 60_000, 10);
    cache.set("a", 1);
    expect(cache.get("a")).toBe(1);
    await vi.advanceTimersByTimeAsync(250);
    // A reload loads the module again from storage.
    vi.resetModules();
    const second = await import("./lookupCache");
    expect(second.createLookupCache<number>("t", 60_000, 10).get("a")).toBe(1);
  });

  it("forgets answers past their lifetime and the oldest past the size", async () => {
    const { createLookupCache } = await import("./lookupCache");
    const cache = createLookupCache<number>("t", 1_000, 2);
    cache.set("a", 1);
    cache.set("b", 2);
    cache.set("c", 3);
    expect(cache.get("a")).toBeUndefined();
    expect(cache.get("c")).toBe(3);
    vi.advanceTimersByTime(1_000);
    expect(cache.get("b")).toBeUndefined();
  });

  it("clears every cache on sign-out", async () => {
    const { clearLookupCaches, createLookupCache } = await import("./lookupCache");
    const cache = createLookupCache<number>("t", 60_000, 10);
    cache.set("a", 1);
    await vi.advanceTimersByTimeAsync(250);
    expect(storage.data.size).toBe(1);
    const { lookupGeneration } = await import("./lookupCache");
    const before = lookupGeneration();
    clearLookupCaches();
    expect(lookupGeneration()).toBe(before + 1);
    expect(cache.get("a")).toBeUndefined();
    expect(storage.data.size).toBe(0);
  });

  it("works in memory when storage is unavailable", async () => {
    vi.stubGlobal("sessionStorage", {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
      removeItem: () => {
        throw new Error("blocked");
      },
    });
    const { createLookupCache } = await import("./lookupCache");
    const cache = createLookupCache<number>("t", 60_000, 10);
    cache.set("a", 1);
    await vi.advanceTimersByTimeAsync(250);
    expect(cache.get("a")).toBe(1);
  });
});
