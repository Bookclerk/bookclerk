import { describe, expect, it, vi } from "vitest";
import { isStoreSamplePath, pauseBookFor, sampleSources } from "@/lib/sample";
import type { TitleMetaQuery } from "@/lib/api";
import type { PlayerState } from "@/lib/player";

const query: TitleMetaQuery = { title: "Mistborn", isbn: "9780000000001" };

async function all(gen: AsyncGenerator<string>): Promise<string[]> {
  const out: string[] = [];
  for await (const url of gen) out.push(url);
  return out;
}

describe("sampleSources", () => {
  it("tries the held edition before asking a store", async () => {
    const lookup = vi.fn(async () => "/api/samples/abc");
    const gen = sampleSources("ed 1", query, lookup);
    expect((await gen.next()).value).toBe("/api/library/editions/ed%201/sample");
    expect(lookup).not.toHaveBeenCalled();
    expect((await gen.next()).value).toBe("/api/samples/abc");
    expect(lookup).toHaveBeenCalledWith(query);
    expect((await gen.next()).done).toBe(true);
  });

  it("asks the stores straight away for a title not held", async () => {
    expect(await all(sampleSources(null, query, async () => "/api/samples/t"))).toEqual([
      "/api/samples/t",
    ]);
  });

  it("yields nothing when the store has none", async () => {
    expect(await all(sampleSources(undefined, query, async () => null))).toEqual([]);
  });

  it("passes a failed lookup on, so it can be retried", async () => {
    const gen = sampleSources("ed", query, async () => {
      throw new Error("offline");
    });
    expect((await gen.next()).value).toBe("/api/library/editions/ed/sample");
    await expect(gen.next()).rejects.toThrow("offline");
  });

  it("never asks the stores without a title", async () => {
    const lookup = vi.fn(async () => "/api/samples/t");
    expect(await all(sampleSources(undefined, { title: "  " }, lookup))).toEqual([]);
    expect(lookup).not.toHaveBeenCalled();
  });

  it("drops a path that is not a same-origin store sample", async () => {
    expect(
      await all(sampleSources(undefined, query, async () => "https://evil.example/a.mp3")),
    ).toEqual([]);
  });
});

describe("isStoreSamplePath", () => {
  it("accepts only single-segment sample paths", () => {
    expect(isStoreSamplePath("/api/samples/abc.def")).toBe(true);
    expect(isStoreSamplePath("/api/samples/")).toBe(false);
    expect(isStoreSamplePath("//evil.example/api/samples/a")).toBe(false);
    expect(isStoreSamplePath("/api/samples/a/../../x")).toBe(false);
    expect(isStoreSamplePath("/api/samples/a?x=1")).toBe(false);
    expect(isStoreSamplePath("/api/samples/a b")).toBe(false);
  });
});

describe("pauseBookFor", () => {
  const player = (status: PlayerState["status"]) => ({
    state: { status } as PlayerState,
    pause: vi.fn(),
  });

  it("pauses a book that is playing or about to", () => {
    for (const status of ["playing", "loading"] as const) {
      const p = player(status);
      pauseBookFor(p);
      expect(p.pause).toHaveBeenCalledOnce();
    }
  });

  it("leaves a paused or idle player alone", () => {
    for (const status of ["paused", "idle", "error"] as const) {
      const p = player(status);
      pauseBookFor(p);
      expect(p.pause).not.toHaveBeenCalled();
    }
  });
});
