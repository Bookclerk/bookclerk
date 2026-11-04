/**
 * Unit tests for the Audnexus request planning and response mapping.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DEFAULT_LIMIT,
  MAX_CHAPTERS,
  MAX_DEPTH,
  MAX_GENRES,
  MAX_LIMIT,
  MAX_PEOPLE,
  MAX_TEXT,
  ProviderFailure,
  asinRequests,
  bookUrl,
  chaptersFromBody,
  chaptersUrl,
  clampLimit,
  normalizeAsin,
  normalizeRegion,
  notFound,
  recordFromBook,
} from "../src/audnexus.ts";
import { VERSION } from "../src/version.ts";

/**
 * Reads a JSON fixture beside this file.
 *
 * @param name - File name under `fixtures/`.
 * @returns The parsed fixture.
 */
function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(new URL(`fixtures/${name}`, import.meta.url), "utf8")) as Record<
    string,
    unknown
  >;
}

const US = { asin: "B002V0QK4C", region: "us" } as const;

test("VERSION matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  assert.match(manifest, new RegExp(`^version = "${VERSION.replace(/\./g, "\\.")}"$`, "m"));
});

test("normalizeRegion maps gb to uk and unknown markets to us", () => {
  assert.equal(normalizeRegion(" DE "), "de");
  assert.equal(normalizeRegion("gb"), "uk");
  assert.equal(normalizeRegion("br"), "us");
  assert.equal(normalizeRegion(""), "us");
  assert.equal(normalizeRegion(undefined), "us");
});

test("normalizeAsin accepts ten letters or digits", () => {
  assert.equal(normalizeAsin(" b002v0qk4c "), "B002V0QK4C");
  assert.equal(normalizeAsin("1094100765"), "1094100765");
  assert.equal(normalizeAsin("B002V0QK4"), undefined);
  assert.equal(normalizeAsin("B002V0QK4C/"), undefined);
  assert.equal(normalizeAsin("../../etc1"), undefined);
});

test("clampLimit defaults, floors and caps", () => {
  assert.equal(clampLimit(undefined), DEFAULT_LIMIT);
  assert.equal(clampLimit(0), DEFAULT_LIMIT);
  assert.equal(clampLimit(Number.POSITIVE_INFINITY), DEFAULT_LIMIT);
  assert.equal(clampLimit(2.7), 2);
  assert.equal(clampLimit(99), MAX_LIMIT);
});

test("asinRequests keeps distinct valid ASINs with their regions", () => {
  const requests = asinRequests(
    [
      { scheme: "isbn", value: "9781423377962" },
      { scheme: "asin", value: "b002v0qk4c" },
      { scheme: "asin", value: "B002V0QK4C" },
      { scheme: "asin", value: "B002V0QK4C", marketplace: "gb" },
      { scheme: "asin", value: "nope" },
      { scheme: "asin", value: "B000000001" },
    ],
    "de",
    3,
  );
  assert.deepEqual(requests, [
    { asin: "B002V0QK4C", region: "de" },
    { asin: "B002V0QK4C", region: "uk" },
    { asin: "B000000001", region: "de" },
  ]);
  assert.deepEqual(asinRequests([{ scheme: "isbn", value: "9781423377962" }], undefined, 5), []);
});

test("asinRequests refuses an empty identifier list", () => {
  assert.throws(
    () => asinRequests([], "us", 1),
    (err: unknown) => err instanceof ProviderFailure && err.code === "invalid_params",
  );
});

test("URLs name the ASIN and region", () => {
  assert.equal(bookUrl(US), "https://api.audnex.us/books/B002V0QK4C?region=us");
  assert.equal(chaptersUrl(US), "https://api.audnex.us/books/B002V0QK4C/chapters?region=us");
});

test("recordFromBook maps an Audnexus book", () => {
  const record = recordFromBook(fixture("book.json"), US);
  assert.ok(record);
  assert.deepEqual(record, {
    title: "Wizard's First Rule",
    subtitle: "Sword of Truth, Book 1",
    description:
      "The masterpiece that started Terry Goodkind's New York Times bestselling epic Sword of Truth In the aftermath of the bru",
    coverUrl: "https://m.media-amazon.com/images/I/91VIQAWhp+L.jpg",
    publisher: "Brilliance Audio",
    lengthMinutes: 2046,
    publishedAt: "2008-10-15",
    categories:
      "Literature & Fiction; Science Fiction & Fantasy; Genre Fiction; Movie, TV & Video Game Tie-Ins; Sagas; Fantasy; Action & Adventure; Epic",
    language: "english",
    abridgement: "unabridged",
    workKind: "book",
    expressionKind: "narration",
    mediaKind: "audiobook",
    identifiers: [
      { scheme: "asin", value: "B002V0QK4C", marketplace: "us" },
      { scheme: "isbn", value: "9781423377962" },
    ],
    credits: [
      { name: "Terry Goodkind", role: "author", level: "roleDefault" },
      { name: "Sam Tsoutsouvas", role: "narrator", level: "roleDefault" },
    ],
    seriesEntries: [{ name: "Sword of Truth", position: "1", storeSeriesId: "B005NB27MK" }],
    ratingOverall: 4.5,
  });
});

test("recordFromBook falls back to the summary and orders genres first", () => {
  const record = recordFromBook(
    {
      asin: "B000000001",
      title: "Short",
      subtitle: "Short",
      summary: "<p>Summary</p>",
      formatType: "abridged",
      isbn: 9781423377962,
      genres: [
        { name: "Tagged", type: "tag" },
        { name: "Genre" },
        { name: "Genre", type: "genre" },
        { name: "Other", type: "ladder" },
        { type: "genre" },
      ],
      seriesPrimary: { name: "Saga", position: 2, asin: "bad" },
      seriesSecondary: { position: "3" },
      rating: 0,
      runtimeLengthMin: "61",
      releaseDate: "15 Oct 2008",
      image: "http://insecure.example/cover.jpg",
      language: "  ",
    },
    { asin: "B000000001", region: "uk" },
  );
  assert.ok(record);
  assert.equal(record.subtitle, undefined);
  assert.equal(record.description, "<p>Summary</p>");
  assert.equal(record.abridgement, "abridged");
  assert.equal(record.categories, "Genre; Tagged");
  assert.deepEqual(record.seriesEntries, [{ name: "Saga", position: "2" }]);
  assert.deepEqual(record.identifiers, [
    { scheme: "asin", value: "B000000001", marketplace: "uk" },
    { scheme: "isbn", value: "9781423377962" },
  ]);
  assert.equal(record.lengthMinutes, 61);
  for (const key of ["ratingOverall", "publishedAt", "coverUrl", "language"] as const) {
    assert.equal(record[key], undefined, key);
  }
});

test("recordFromBook ignores fields of the wrong type", () => {
  const record = recordFromBook(
    {
      asin: "B000000001",
      title: "Odd",
      authors: "Someone",
      narrators: [{ name: "" }, null, { name: "N" }, { name: "N" }],
      genres: "Fantasy",
      formatType: 3,
      isbn: "12345",
      rating: "high",
      runtimeLengthMin: -5,
    },
    US,
  );
  assert.ok(record);
  assert.deepEqual(record.credits, [{ name: "N", role: "narrator", level: "roleDefault" }]);
  assert.equal(record.abridgement, "unknown");
  assert.equal(record.categories, undefined);
  assert.equal(record.identifiers.length, 1);
  assert.equal(record.ratingOverall, undefined);
  assert.equal(record.lengthMinutes, undefined);
});

test("recordFromBook caps long text and long lists", () => {
  const record = recordFromBook(
    {
      asin: "B000000001",
      title: "t".repeat(MAX_TEXT + 10),
      authors: Array.from({ length: 50 }, (_, i) => ({ name: `A${i}` })),
      genres: Array.from({ length: 50 }, (_, i) => ({ name: `G${i}`, type: "genre" })),
    },
    US,
  );
  assert.ok(record);
  assert.equal(record.title.length, MAX_TEXT);
  assert.equal(record.credits.length, MAX_PEOPLE);
  assert.equal(record.categories?.split("; ").length, MAX_GENRES);
});

test("recordFromBook drops bodies without an ASIN or title", () => {
  for (const body of [
    null,
    [],
    { title: "No ASIN" },
    { asin: "B000000001" },
    { asin: "bad", title: "T" },
  ]) {
    assert.equal(recordFromBook(body, US), undefined);
  }
});

test("chaptersFromBody maps an Audnexus layout", () => {
  assert.deepEqual(chaptersFromBody(fixture("chapters.json")), {
    found: true,
    accurate: true,
    brandIntroMs: 3970,
    brandOutroMs: 4945,
    runtimeMs: 122790390,
    chapters: [
      { title: "Opening Credits", startMs: 0, lengthMs: 24481, depth: 0 },
      { title: "Chapter 1", startMs: 24481, lengthMs: 917551, depth: 0 },
      { title: "Chapter 2", startMs: 942032, lengthMs: 1925073, depth: 0 },
      { title: "Chapter 3", startMs: 2867105, lengthMs: 2504086, depth: 0 },
    ],
  });
});

test("chaptersFromBody flattens nested chapters in pre-order", () => {
  const layout = chaptersFromBody({
    chapters: [
      {
        title: "Part One",
        startOffsetMs: 0,
        chapters: [
          { title: "Chapter 1", startOffsetMs: 0, lengthMs: 10 },
          { title: "Chapter 2", startOffsetMs: 10, lengthMs: 0 },
        ],
      },
      { startOffsetMs: 20 },
      { title: "No start" },
      "junk",
    ],
  });
  assert.deepEqual(layout, {
    found: true,
    accurate: false,
    chapters: [
      { title: "Part One", startMs: 0, depth: 0 },
      { title: "Chapter 1", startMs: 0, lengthMs: 10, depth: 1 },
      { title: "Chapter 2", startMs: 10, depth: 1 },
      { title: "", startMs: 20, depth: 0 },
    ],
  });
});

test("chaptersFromBody caps the count and the depth", () => {
  const many = chaptersFromBody({
    chapters: Array.from({ length: MAX_CHAPTERS + 50 }, (_, i) => ({
      title: `C${i}`,
      startOffsetMs: i,
    })),
  });
  assert.equal(many.chapters.length, MAX_CHAPTERS);

  let nested: Record<string, unknown> = { title: "leaf", startOffsetMs: 0 };
  for (let i = 0; i < MAX_DEPTH + 5; i += 1)
    nested = { title: `d${i}`, startOffsetMs: 0, chapters: [nested] };
  const deep = chaptersFromBody({ chapters: [nested] });
  assert.equal(Math.max(...deep.chapters.map((c) => c.depth)), MAX_DEPTH);
});

test("chaptersFromBody without chapters is not found", () => {
  for (const body of [null, {}, { chapters: [] }, { chapters: "x" }, []]) {
    assert.deepEqual(chaptersFromBody(body), notFound());
  }
});
