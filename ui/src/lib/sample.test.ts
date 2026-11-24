import { describe, expect, it, vi } from "vitest";
import {
  findSample,
  isStoreSamplePath,
  pauseBookFor,
  readSample,
  sampleSourceKey,
  storeSampleFallback,
} from "@/lib/sample";
import type { TitleMetaQuery } from "@/lib/api";
import type { PlayerState } from "@/lib/player";

const query: TitleMetaQuery = { title: "Mistborn", isbn: "9780000000001" };

describe("findSample", () => {
  it("offers the held edition's sample without asking a store", async () => {
    const lookup = vi.fn(async () => ({ edition_sample_path: "/anything/the/server/said" }));
    expect(await findSample("ed 1", query, lookup)).toEqual({
      url: "/api/library/editions/ed%201/sample",
      kind: "edition",
    });
    expect(lookup).toHaveBeenCalledWith({ ...query, edition_id: "ed 1" });
  });

  it("offers a store's sample when the edition gives none", async () => {
    const lookup = vi.fn(async () => ({ sample_path: "/api/samples/abc" }));
    expect(await findSample("ed", query, lookup)).toEqual({
      url: "/api/samples/abc",
      kind: "store",
    });
    expect(await findSample(null, query, lookup)).toEqual({
      url: "/api/samples/abc",
      kind: "store",
    });
    expect(lookup).toHaveBeenLastCalledWith({ ...query, edition_id: null });
  });

  it("offers nothing when no sample can be played here", async () => {
    expect(await findSample("ed", query, async () => ({}))).toBeNull();
    // An edition path for a title not held is not taken either.
    expect(await findSample(null, query, async () => ({ edition_sample_path: "/x" }))).toBeNull();
  });

  it("drops a path that is not a same-origin store sample", async () => {
    expect(
      await findSample(undefined, query, async () => ({
        sample_path: "https://evil.example/a.mp3",
      })),
    ).toBeNull();
  });

  it("passes a failed lookup on", async () => {
    await expect(
      findSample("ed", query, async () => {
        throw new Error("offline");
      }),
    ).rejects.toThrow("offline");
  });

  it("never asks without a title or identifier", async () => {
    const lookup = vi.fn(async () => ({ sample_path: "/api/samples/t" }));
    expect(await findSample(undefined, { title: "  " }, lookup)).toBeNull();
    expect(lookup).not.toHaveBeenCalled();
  });
});

describe("storeSampleFallback", () => {
  it("asks the stores without the edition", async () => {
    const lookup = vi.fn(async () => ({ sample_path: "/api/samples/s" }));
    expect(await storeSampleFallback(query, lookup)).toEqual({
      url: "/api/samples/s",
      kind: "store",
    });
    expect(lookup).toHaveBeenCalledWith({ ...query, edition_id: null });
    expect(await storeSampleFallback(query, async () => ({}))).toBeNull();
  });
});

describe("readSample", () => {
  it("prefers the edition's sample for the edition asked about", () => {
    expect(readSample({ edition_sample_path: "/e", sample_path: "/api/samples/s" }, "ed")).toEqual({
      url: "/api/library/editions/ed/sample",
      kind: "edition",
    });
    expect(readSample({ sample_path: "/api/samples/s" }, "ed")?.kind).toBe("store");
    expect(readSample({ sample_path: " " }, "ed")).toBeNull();
  });
});

describe("sampleSourceKey", () => {
  const query: TitleMetaQuery = { title: "Elantris", asin: "B000000001" };

  it("changes when enrichment adds a held edition or an identifier", () => {
    const before = sampleSourceKey(null, query);
    expect(sampleSourceKey(null, { ...query, title: "Elantris (10th)" })).toBe(before);
    expect(sampleSourceKey("ed-1", query)).not.toBe(before);
    expect(sampleSourceKey(null, { ...query, isbn: "9780000000001" })).not.toBe(before);
    expect(
      sampleSourceKey(null, {
        ...query,
        store_editions: [{ source: "libro", product_id: "9780000000001" }],
      }),
    ).not.toBe(before);
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
