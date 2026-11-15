import { describe, expect, it } from "vitest";
import { acquirableEditions, type Edition, type WorkDetail } from "@/lib/api";
import { titleDetailFromWork } from "@/lib/titleDetail";

/**
 * An edition with one held listing.
 *
 * @param id - Edition id.
 * @param extra - Fields to override.
 * @returns The edition.
 */
function edition(id: string, extra: Partial<Edition> = {}): Edition {
  return {
    id,
    work_id: "w",
    expression_id: "x",
    expression_kind: "narration",
    language: "en",
    abridgement: "unabridged",
    media_kind: "audio",
    title: "Dune",
    subtitle: null,
    published_at: "2019-01-01",
    duration_minutes: 1260,
    cover_path: null,
    acquire_status: "not_acquired",
    listings: [
      {
        id: `l-${id}`,
        plugin_key: "audible",
        marketplace: "us",
        product_id: "B002V1OF70",
        url: null,
        holdings: [
          {
            id: `h-${id}`,
            holder: { kind: "account", plugin_key: "audible", account_id: "acct" },
            entitlement: "purchase",
            purchased_at: null,
            added_at: "2026-11-15T00:00:00Z",
          },
        ],
      },
    ],
    ...extra,
  };
}

/**
 * A work with `editions`.
 *
 * @param editions - Its editions.
 * @returns The work.
 */
function work(editions: Edition[]): WorkDetail {
  return {
    id: "w",
    kind: "book",
    title: "Dune",
    description: "A desert planet.",
    authors: ["Frank Herbert", "Brian Herbert"],
    series: "Dune",
    series_position: "1",
    editions,
    state: {
      is_finished: false,
      rating_overall: 4,
      rating_performance: null,
      rating_story: null,
      tags: "scifi",
    },
  };
}

describe("titleDetailFromWork", () => {
  it("takes edition fields from audio editions first", () => {
    const detail = titleDetailFromWork(
      work([
        edition("print", {
          media_kind: "print",
          subtitle: "Paperback",
          abridgement: "abridged",
          listings: [
            {
              id: "lp",
              plugin_key: "libro",
              marketplace: "us",
              product_id: "9780441013593",
              url: null,
              holdings: [],
            },
          ],
        }),
        edition("audio", { acquire_status: "acquired", cover_path: "/api/covers/t" }),
      ]),
      "acquired",
    );
    expect(detail).toMatchObject({
      title: "Dune",
      subtitle: "Paperback",
      authors: "Frank Herbert, Brian Herbert",
      series_index: "1",
      asin: "B002V1OF70",
      isbn: "9780441013593",
      cover_path: "/api/covers/t",
      library_edition_id: "audio",
      work_id: "w",
      length_minutes: 1260,
      is_abridged: false,
      tags: "scifi",
      rating_overall: 4,
      acquire_status: "acquired",
      acquirable_editions: 0,
      source: "audible",
      content_kind: "book",
      showCommerce: false,
    });
  });

  it("counts held editions left to acquire beside an acquired one", () => {
    const detail = titleDetailFromWork(
      work([
        edition("audio", { acquire_status: "acquired" }),
        edition("ebook", { media_kind: "ebook" }),
      ]),
      "acquired",
    );
    expect(detail.acquire_status).toBe("acquired");
    expect(detail.acquirable_editions).toBe(1);
  });

  it("leaves unknown fields empty", () => {
    const detail = titleDetailFromWork(
      {
        ...work([edition("e", { abridgement: "unknown", listings: [] })]),
        authors: [],
        state: null,
      },
      "not_acquired",
    );
    expect(detail.authors).toBeNull();
    expect(detail.is_abridged).toBeNull();
    expect(detail.asin).toBeNull();
    expect(detail.library_edition_id).toBeNull();
    expect(detail.source).toBeNull();
  });
});

describe("titleDetailFromWork state", () => {
  it("carries the caller's own state and whether they can edit it", () => {
    const detail = titleDetailFromWork({ ...work([edition("e")]), can_edit_state: true });
    expect(detail.user_state?.tags).toBe("scifi");
    expect(detail.can_edit_state).toBe(true);
  });

  it("cannot edit when the server does not say so", () => {
    const detail = titleDetailFromWork({ ...work([edition("e")]), state: null });
    expect(detail.user_state).toBeNull();
    expect(detail.can_edit_state).toBe(false);
  });
});

describe("acquirableEditions", () => {
  it("names held editions that are not acquired or on their way", () => {
    const editions = [
      edition("todo"),
      edition("failed", { acquire_status: "error" }),
      edition("done", { acquire_status: "acquired" }),
      edition("queued", { acquire_status: "queued" }),
      edition("downloading", { acquire_status: "downloading" }),
      edition("unheld", { listings: [] }),
    ];
    expect(acquirableEditions(work(editions))).toEqual(["todo", "failed"]);
  });
});
