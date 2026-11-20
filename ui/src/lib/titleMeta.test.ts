import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { fetchTitleMetaBatch, type StoreEdition } from "./api";

/**
 * Stubs `fetch` to answer every title-meta batch with one `null` per query.
 *
 * @returns The stub, which records each call.
 */
function stubFetch() {
  const fetchMock = vi.fn(async (_url: string, init: RequestInit) => {
    const sent = JSON.parse(String(init.body)) as { queries: unknown[] };
    return new Response(JSON.stringify({ results: sent.queries.map(() => null) }), {
      status: 200,
    });
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

/**
 * The queries one recorded batch call sent.
 *
 * @param fetchMock - The stub from {@link stubFetch}.
 * @returns The parsed `queries` array.
 */
function sentQueries(fetchMock: ReturnType<typeof stubFetch>): Record<string, unknown>[] {
  const body = fetchMock.mock.calls[0]![1].body;
  return (JSON.parse(String(body)) as { queries: Record<string, unknown>[] }).queries;
}

beforeEach(() => {
  vi.stubGlobal("window", globalThis);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("fetchTitleMetaBatch", () => {
  it("sends the editions that listed the title, without blank ones", async () => {
    const fetchMock = stubFetch();
    await fetchTitleMetaBatch([
      {
        title: "Mistborn (edition test)",
        store_editions: [
          { source: "graphicaudio", product_id: "1716" },
          { source: "", product_id: "orphan" },
          { source: "chirp", product_id: " " },
        ],
      },
    ]);
    expect(sentQueries(fetchMock)[0].store_editions).toEqual([
      { source: "graphicaudio", product_id: "1716" },
    ]);
  });

  it("caps the editions at the server's limit and leaves none out when absent", async () => {
    const fetchMock = stubFetch();
    const many: StoreEdition[] = Array.from({ length: 20 }, (_, i) => ({
      source: "graphicaudio",
      product_id: String(i + 1),
    }));
    await fetchTitleMetaBatch([
      { title: "Many editions (edition test)", store_editions: many },
      { title: "No editions (edition test)" },
    ]);
    const [capped, bare] = sentQueries(fetchMock);
    expect(capped.store_editions).toEqual(many.slice(0, 16));
    expect(bare).not.toHaveProperty("store_editions");
  });
});
