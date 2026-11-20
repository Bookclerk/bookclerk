import { describe, expect, it } from "vitest";
import type { Edition, WorkDetail, WorkSummary } from "./api";
import type { PlaybackProgress } from "./playback";
import {
  SHELF_SIZE,
  percentDone,
  audioEditions,
  playableEdition,
  readShelf,
  rowOffersPlay,
  targetFor,
} from "./playTarget";

function edition(id: string, over: Partial<Edition> = {}): Edition {
  return {
    id,
    work_id: "w1",
    expression_id: "x1",
    expression_kind: "narration",
    language: "en",
    abridgement: "unabridged",
    media_kind: "audio",
    title: `Edition ${id}`,
    subtitle: null,
    published_at: null,
    duration_minutes: 600,
    cover_path: `/api/covers/${id}`,
    acquire_status: "acquired",
    listings: [],
    ...over,
  } as Edition;
}

function work(id: string, editions: Edition[], over: Partial<WorkDetail> = {}): WorkDetail {
  return {
    id,
    kind: "book",
    title: `Work ${id}`,
    description: null,
    authors: ["Ann Author", "Bo Writer"],
    series: null,
    series_position: null,
    editions,
    ...over,
  };
}

function progress(editionId: string, over: Partial<PlaybackProgress> = {}): PlaybackProgress {
  return {
    edition_id: editionId,
    work_id: "w1",
    position_ms: 30_000,
    duration_ms: 120_000,
    is_finished: false,
    started_at: "2026-11-19T10:00:00Z",
    finished_at: null,
    updated_at: "2026-11-19T10:00:00Z",
    ...over,
  };
}

describe("rowOffersPlay", () => {
  const row = (over: Partial<WorkSummary>) =>
    ({ acquire_status: "acquired", media_kinds: ["audio"], ...over }) as WorkSummary;

  it("offers play for acquired audio only", () => {
    expect(rowOffersPlay(row({}))).toBe(true);
    expect(rowOffersPlay(row({ acquire_status: "queued" }))).toBe(false);
    expect(rowOffersPlay(row({ media_kinds: ["ebook"] }))).toBe(false);
  });
});

describe("playableEdition", () => {
  const w = work("w1", [
    edition("e-book", { media_kind: "ebook" }),
    edition("pending", { acquire_status: "not_acquired" }),
    edition("a"),
    edition("b"),
  ]);

  it("lists the acquired audio editions in the work's order", () => {
    expect(audioEditions(w).map((found) => found.id)).toEqual(["a", "b"]);
  });

  it("takes the first acquired audio edition", () => {
    expect(playableEdition(w)?.id).toBe("a");
  });

  it("prefers the edition the listener was last in", () => {
    expect(playableEdition(w, [progress("pending"), progress("b"), progress("a")])?.id).toBe("b");
  });

  it("answers nothing when no audio is acquired", () => {
    expect(playableEdition(work("w2", [edition("e", { media_kind: "print" })]))).toBeNull();
  });
});

describe("targetFor", () => {
  it("shows the work's title and people with the edition's cover", () => {
    const e = edition("a");
    expect(targetFor(work("w1", [e]), e)).toEqual({
      editionId: "a",
      workId: "w1",
      title: "Work w1",
      authors: "Ann Author, Bo Writer",
      coverPath: "/api/covers/a",
    });
  });

  it("falls back to the edition's title and no authors", () => {
    const e = edition("a");
    const target = targetFor(work("w1", [e], { title: "", authors: [] }), e);
    expect(target.title).toBe("Edition a");
    expect(target.authors).toBeNull();
  });
});

describe("readShelf", () => {
  it("lists unfinished books with their works, skipping what cannot be read", async () => {
    const asked: number[] = [];
    const shelf = await readShelf(
      async ({ limit }) => {
        asked.push(limit);
        return {
          items: [
            progress("a", { work_id: "w1" }),
            progress("gone", { work_id: "w-gone" }),
            progress("orphan", { work_id: null }),
            progress("done", { work_id: "w1", is_finished: true }),
            progress("other", { work_id: "w1" }),
            progress("b", { work_id: "w2" }),
          ],
        };
      },
      async (id) => {
        if (id === "w-gone") throw new Error("404");
        return work(id, [edition(id === "w1" ? "a" : "b")]);
      },
    );
    expect(asked).toEqual([SHELF_SIZE]);
    expect(shelf.map((item) => [item.target.workId, item.target.editionId])).toEqual([
      ["w1", "a"],
      ["w2", "b"],
    ]);
    expect(shelf[0].progress.position_ms).toBe(30_000);
  });
});

describe("percentDone", () => {
  it("rounds to a whole percentage inside 0 to 100", () => {
    expect(percentDone(progress("a"))).toBe(25);
    expect(percentDone(progress("a", { position_ms: 999_999 }))).toBe(100);
    expect(percentDone(progress("a", { duration_ms: null }))).toBe(0);
    expect(percentDone(progress("a", { position_ms: 0, is_finished: true }))).toBe(100);
  });
});
