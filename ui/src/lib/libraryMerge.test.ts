import { describe, expect, it } from "vitest";
import type { WorkSummary } from "./api";
import type { WindowEditionView, WindowEntryView } from "./join";
import {
  compareRows,
  mergeLibraryList,
  searchTerms,
  sortAuthor,
  sortSeries,
  sortTitle,
  type LibraryListItem,
  type LibraryMergeOptions,
} from "./libraryMerge";

/**
 * A work of this library.
 *
 * @param id - Its id.
 * @param title - Its title.
 * @param extra - Fields to override.
 * @returns The work.
 */
function work(id: string, title: string, extra: Partial<WorkSummary> = {}): WorkSummary {
  return {
    id,
    kind: "book",
    title,
    authors: null,
    series: null,
    series_position: null,
    cover_edition_id: null,
    cover_path: null,
    media_kinds: ["audio"],
    acquire_status: "acquired",
    edition_count: 1,
    holding_count: 1,
    added_at: "2026-01-01T00:00:00Z",
    ...extra,
  };
}

/**
 * A book another library holds, with one edition.
 *
 * @param key - Its entry key.
 * @param title - Its title.
 * @param extra - Edition fields to override.
 * @param workId - The work of this library it was placed on, if any.
 * @returns The entry.
 */
function entry(
  key: string,
  title: string,
  extra: Partial<WindowEditionView> = {},
  workId: string | null = null,
): WindowEntryView {
  return {
    key,
    work_id: workId,
    editions: [
      {
        library_id: "lib-a",
        owner_origin: "https://a.example",
        store: "libro",
        marketplace: "us",
        product_id: `p-${key}`,
        formats: ["audio"],
        title,
        yours: false,
        ...extra,
      },
    ],
  };
}

const OPTIONS: LibraryMergeOptions = {
  sort: "title",
  q: "",
  status: "all",
  media: "all",
  facetApplied: false,
  place: "all",
  complete: true,
};

/**
 * The rows as short labels: a work's id, or `@` and an entry's key.
 *
 * @param items - The merged rows.
 * @returns Their labels.
 */
function labels(items: LibraryListItem[]): string[] {
  return items.map((item) => (item.kind === "work" ? item.work.id : `@${item.entry.key}`));
}

describe("sort keys", () => {
  it("match the works index", () => {
    expect(sortTitle("  The Hobbit ")).toBe("hobbit");
    expect(sortTitle("A")).toBe("a");
    expect(sortTitle("An Unkindness")).toBe("unkindness");
    expect(sortAuthor("Frank Herbert, Brian Herbert")).toBe("herbert, frank");
    expect(sortAuthor("Plato")).toBe("plato");
    expect(sortAuthor(null)).toBe("");
    expect(sortSeries(null, "1")).toBe("1");
    expect(sortSeries("Dune", "Book 2")).toBe("0dune\u00010000000002");
    expect(sortSeries("Dune", "#003.5")).toBe("0dune\u00010000000003.5");
    expect(sortSeries("Dune", "00")).toBe("0dune\u00010000000000");
    expect(sortSeries(" Dune ", null)).toBe("0dune\u0001~");
  });

  it("order unknown runtimes and dates last", () => {
    const base = { title: "x", authors: null, series: null, position: null };
    const row = (length: number | null, added: string | null) => ({
      ...base,
      length,
      added,
      purchased: added,
    });
    expect(compareRows("length", row(90, null), row(60, null))).toBeLessThan(0);
    expect(compareRows("length", row(0, null), row(60, null))).toBeGreaterThan(0);
    expect(compareRows("length", row(null, null), row(0, null))).toBe(0);
    expect(compareRows("added", row(null, "2026-02"), row(null, "2026-01"))).toBeLessThan(0);
    expect(compareRows("purchased", row(null, null), row(null, "2026-01"))).toBeGreaterThan(0);
    expect(compareRows("purchased", row(null, "2026-01"), row(null, null))).toBeLessThan(0);
    expect(compareRows("added", row(null, "2026-01"), row(null, "2026-01"))).toBe(0);
    expect(
      compareRows(
        "author",
        { ...row(null, null), authors: "Jane Austen", title: "Emma" },
        { ...row(null, null), authors: "Jane Austen", title: "Persuasion" },
      ),
    ).toBeLessThan(0);
    expect(
      compareRows(
        "series",
        { ...row(null, null), series: "Dune", position: "2", title: "A" },
        { ...row(null, null), series: null, title: "B" },
      ),
    ).toBeLessThan(0);
  });
});

describe("searchTerms", () => {
  it("splits fields, phrases and words", () => {
    expect(searchTerms('author:"Jane Austen" emma series:x')).toEqual([
      { field: "author", text: "jane austen" },
      { field: null, text: "emma" },
      { field: "series", text: "x" },
    ]);
    expect(searchTerms('"" genre:sci')).toEqual([{ field: null, text: "genre:sci" }]);
    expect(searchTerms("   ")).toEqual([]);
  });
});

describe("mergeLibraryList", () => {
  const alice = work("w-alice", "Alice in Wonderland");
  const moby = work("w-moby", "Moby Dick");
  const war = work("w-war", "The War of the Worlds");
  const emma = entry("emma", "Emma", { authors: "Jane Austen" });
  const kim = entry("kim", "Kim", { formats: ["audio", "pdf"], yours: true });
  const zen = entry("zen", "Zen", { owner_origin: "https://b.example" });
  const mobyThere = entry("moby-a", "Moby-Dick", {}, "w-moby");

  it("places other libraries' books among the works in title order", () => {
    const items = mergeLibraryList([alice, moby, war], [zen, kim, emma, mobyThere], OPTIONS);
    expect(labels(items)).toEqual(["w-alice", "@emma", "@kim", "w-moby", "w-war", "@zen"]);
    // A book placed on a work joins that work's row.
    const mobyRow = items.find((item) => item.kind === "work" && item.work.id === "w-moby");
    expect(mobyRow?.kind === "work" ? mobyRow.elsewhere : []).toEqual([mobyThere]);
  });

  it("shows only what sorts before the last work while pages remain", () => {
    const items = mergeLibraryList([alice, moby], [zen, kim, emma], {
      ...OPTIONS,
      complete: false,
    });
    expect(labels(items)).toEqual(["w-alice", "@emma", "@kim", "w-moby"]);
    expect(labels(mergeLibraryList([], [emma], { ...OPTIONS, complete: false }))).toEqual([]);
    expect(labels(mergeLibraryList([], [emma], OPTIONS))).toEqual(["@emma"]);
  });

  it("gives a book its own row when its work is not shown", () => {
    expect(labels(mergeLibraryList([alice], [mobyThere], OPTIONS))).toEqual(["w-alice", "@moby-a"]);
  });

  it("puts other libraries' books last in date orders", () => {
    const items = mergeLibraryList([war, alice], [emma], { ...OPTIONS, sort: "added" });
    expect(labels(items)).toEqual(["w-war", "w-alice", "@emma"]);
    const long = entry("long", "Long", { length_minutes: 900 });
    const byLength = mergeLibraryList(
      [work("w-1", "One", { length_minutes: 1000 }), work("w-2", "Two", { length_minutes: 60 })],
      [long, emma],
      { ...OPTIONS, sort: "length" },
    );
    expect(labels(byLength)).toEqual(["w-1", "@long", "w-2", "@emma"]);
  });

  it("follows this library's matches under a search", () => {
    const q = { ...OPTIONS, q: "author:austen" };
    expect(labels(mergeLibraryList([war], [kim, emma], q))).toEqual(["w-war", "@emma"]);
    expect(labels(mergeLibraryList([war], [emma], { ...q, complete: false }))).toEqual(["w-war"]);
    expect(labels(mergeLibraryList([], [kim, emma], { ...OPTIONS, q: "k" }))).toEqual(["@kim"]);
    expect(labels(mergeLibraryList([], [kim], { ...OPTIONS, q: "asin:p-kim" }))).toEqual(["@kim"]);
  });

  it("applies the filters a remote book can answer", () => {
    expect(labels(mergeLibraryList([], [kim, emma], { ...OPTIONS, status: "queued" }))).toEqual([]);
    expect(labels(mergeLibraryList([], [kim, emma], { ...OPTIONS, facetApplied: true }))).toEqual(
      [],
    );
    expect(labels(mergeLibraryList([], [kim, emma], { ...OPTIONS, media: "ebook" }))).toEqual([
      "@kim",
    ]);
    expect(labels(mergeLibraryList([], [kim, emma], { ...OPTIONS, media: "audio" }))).toEqual([
      "@emma",
      "@kim",
    ]);
    expect(labels(mergeLibraryList([], [kim], { ...OPTIONS, media: "print" }))).toEqual([]);
  });

  it("narrows by where books are held", () => {
    const all = [alice, moby];
    const others = [emma, kim, zen, mobyThere];
    const at = (place: LibraryMergeOptions["place"]) =>
      labels(mergeLibraryList(all, others, { ...OPTIONS, place }));
    expect(at("this")).toEqual(["w-alice", "w-moby"]);
    expect(at("other")).toEqual(["@emma", "@kim", "w-moby", "@zen"]);
    expect(at("yours")).toEqual(["@kim"]);
    expect(at("host:b.example")).toEqual(["@zen"]);
    expect(at("host:a.example")).toEqual(["@emma", "@kim", "w-moby"]);
  });
});
