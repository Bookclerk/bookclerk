import { afterEach, describe, expect, it, vi } from "vitest";
import { createLookupBatcher } from "./lookupBatcher";

/**
 * A sender that records each batch and answers after `release()`.
 *
 * @returns The sender, its recorded batches and its release switch.
 */
function gatedSender() {
  const batches: string[][] = [];
  let release: () => void = () => {};
  let gate = new Promise<void>((resolve) => (release = resolve));
  return {
    batches,
    release: () => {
      release();
      gate = new Promise<void>((resolve) => (release = resolve));
    },
    send: async (queries: string[]) => {
      batches.push(queries);
      await gate;
      return queries.map((q) => q.toUpperCase());
    },
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("createLookupBatcher", () => {
  it("sends lookups asked within the window as one batch, split at the limit", async () => {
    vi.useFakeTimers();
    const sent: string[][] = [];
    const lookup = createLookupBatcher<string, string>({
      maxBatch: 3,
      windowMs: 50,
      keyOf: (q) => q,
      send: async (qs) => {
        sent.push(qs);
        return qs.map((q) => q.toUpperCase());
      },
    });
    const answers = Promise.all(["a", "b", "c", "d"].map(lookup));
    await vi.advanceTimersByTimeAsync(50);
    expect(await answers).toEqual(["A", "B", "C", "D"]);
    expect(sent).toEqual([["a", "b", "c"], ["d"]]);
  });

  it("keeps one batch in flight and gathers what is asked meanwhile", async () => {
    vi.useFakeTimers();
    const sender = gatedSender();
    const lookup = createLookupBatcher<string, string>({
      maxBatch: 24,
      windowMs: 10,
      keyOf: (q) => q,
      send: sender.send,
    });
    const first = lookup("a");
    await vi.advanceTimersByTimeAsync(10);
    const later = [lookup("b"), lookup("c")];
    await vi.advanceTimersByTimeAsync(100);
    // "b" and "c" wait for "a"'s batch rather than opening their own.
    expect(sender.batches).toEqual([["a"]]);
    sender.release();
    expect(await first).toBe("A");
    await vi.advanceTimersByTimeAsync(0);
    expect(sender.batches).toEqual([["a"], ["b", "c"]]);
    sender.release();
    expect(await Promise.all(later)).toEqual(["B", "C"]);
  });

  it("asks once for the same key and rejects a failed batch's lookups", async () => {
    vi.useFakeTimers();
    let calls = 0;
    const lookup = createLookupBatcher<string, string>({
      maxBatch: 24,
      windowMs: 10,
      keyOf: (q) => q,
      send: async () => {
        calls += 1;
        throw new Error("down");
      },
    });
    const one = lookup("a");
    const two = lookup("a");
    expect(two).toBe(one);
    const settled = expect(one).rejects.toThrow("down");
    await vi.advanceTimersByTimeAsync(10);
    await settled;
    expect(calls).toBe(1);
  });

  it("does not share a lookup across viewer generations", async () => {
    vi.useFakeTimers();
    let generation = 0;
    const lookup = createLookupBatcher<string, string>({
      maxBatch: 24,
      windowMs: 10,
      keyOf: (q) => q,
      generation: () => generation,
      send: async (qs) => qs,
    });
    const before = lookup("a");
    generation += 1;
    const after = lookup("a");
    expect(after).not.toBe(before);
    await vi.advanceTimersByTimeAsync(10);
    expect(await Promise.all([before, after])).toEqual(["a", "a"]);
  });

  it("waits for `before` ahead of each batch", async () => {
    vi.useFakeTimers();
    let open: () => void = () => {};
    const sent: string[][] = [];
    const lookup = createLookupBatcher<string, string>({
      maxBatch: 24,
      windowMs: 10,
      keyOf: (q) => q,
      before: () => new Promise<void>((resolve) => (open = resolve)),
      send: async (qs) => {
        sent.push(qs);
        return qs;
      },
    });
    const a = lookup("a");
    await vi.advanceTimersByTimeAsync(10);
    const b = lookup("b");
    await vi.advanceTimersByTimeAsync(0);
    expect(sent).toEqual([]);
    open();
    // A lookup asked during the wait joins the batch.
    expect(await Promise.all([a, b])).toEqual(["a", "b"]);
    expect(sent).toEqual([["a", "b"]]);
  });
});
