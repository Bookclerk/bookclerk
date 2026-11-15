import { describe, expect, it } from "vitest";
import type { WorkSummary, WorksPage, WorksQuery } from "@/lib/api";
import { MAX_WORKS_PAGE, metaSearchQuery, readWorks } from "@/lib/libraryWorks";

/**
 * A work row named `id`.
 *
 * @param id - Its id and title.
 * @returns The row.
 */
function work(id: string): WorkSummary {
  return {
    id,
    kind: "book",
    title: id,
    authors: null,
    series: null,
    series_position: null,
    cover_edition_id: null,
    cover_path: null,
    media_kinds: ["audio"],
    acquire_status: "not_acquired",
    edition_count: 1,
    holding_count: 1,
    added_at: "2026-11-15T00:00:00Z",
  };
}

/**
 * Serves `pages` in turn, recording each query it was asked.
 *
 * @param pages - The pages to answer with, in order.
 * @returns The recorded queries and the page reader.
 */
function pager(pages: WorksPage[]) {
  const asked: WorksQuery[] = [];
  const fetchPage = async (query: WorksQuery): Promise<WorksPage> => {
    asked.push(query);
    const page = pages.shift();
    if (!page) throw new Error("read past the last page");
    return page;
  };
  return { asked, fetchPage };
}

describe("metaSearchQuery", () => {
  it("searches people and series by field and genres as a phrase", () => {
    expect(metaSearchQuery("authors", "Ursula K. Le Guin")).toBe('author:"Ursula K. Le Guin"');
    expect(metaSearchQuery("narrators", "Scott Brick")).toBe('narrator:"Scott Brick"');
    expect(metaSearchQuery("series", "Earthsea")).toBe('series:"Earthsea"');
    expect(metaSearchQuery("genres", "Science Fiction")).toBe('"Science Fiction"');
  });

  it("keeps quotes and backslashes from ending the phrase", () => {
    expect(metaSearchQuery("series", 'The "Best" \\ Of')).toBe('series:"The  Best    Of"');
  });
});

describe("readWorks", () => {
  it("reads one page when it holds enough", async () => {
    const { asked, fetchPage } = pager([{ works: [work("a"), work("b")], next_cursor: "c1" }]);
    const read = await readWorks(fetchPage, { sort: "title" }, 2);
    expect(read).toEqual({ works: [work("a"), work("b")], next: "c1" });
    expect(asked).toEqual([{ sort: "title", cursor: undefined, limit: 2 }]);
  });

  it("follows cursors through short pages until it has enough", async () => {
    const { asked, fetchPage } = pager([
      { works: [work("a")], next_cursor: "c1" },
      { works: [], next_cursor: "c2" },
      { works: [work("b"), work("c")], next_cursor: "c3" },
    ]);
    const read = await readWorks(fetchPage, { q: "dune" }, 3);
    expect(read.works.map((w) => w.id)).toEqual(["a", "b", "c"]);
    expect(read.next).toBe("c3");
    expect(asked.map((q) => [q.cursor, q.limit])).toEqual([
      [undefined, 3],
      ["c1", 2],
      ["c2", 2],
    ]);
  });

  it("stops at the last page and starts from a given cursor", async () => {
    const { asked, fetchPage } = pager([{ works: [work("z")], next_cursor: null }]);
    const read = await readWorks(fetchPage, {}, 40, "c9");
    expect(read).toEqual({ works: [work("z")], next: null });
    expect(asked).toEqual([{ cursor: "c9", limit: 40 }]);
  });

  it("asks for at most the daemon's page size", async () => {
    const { asked, fetchPage } = pager([{ works: [], next_cursor: null }]);
    await readWorks(fetchPage, {}, 1000);
    expect(asked[0]?.limit).toBe(MAX_WORKS_PAGE);
  });

  it("stops between pages once the query is stale", async () => {
    let current = true;
    const { asked, fetchPage } = pager([
      { works: [work("a")], next_cursor: "c1" },
      { works: [work("b")], next_cursor: "c2" },
    ]);
    const read = await readWorks(
      async (q) => {
        const page = await fetchPage(q);
        current = false;
        return page;
      },
      {},
      5,
      undefined,
      () => current,
    );
    expect(read.works.map((w) => w.id)).toEqual(["a"]);
    expect(asked).toHaveLength(1);
  });
});
