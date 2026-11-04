/**
 * Unit tests for the Open Library request planning and response mapping.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DEFAULT_LIMIT,
  MAX_AUTHORS,
  MAX_LIMIT,
  MAX_SUBJECTS,
  MAX_TEXT,
  ProviderFailure,
  clampLimit,
  normalizeIsbn,
  normalizeOlid,
  planLookup,
  planSearch,
  recordFromDoc,
  recordsFromSearch,
  titlesOverlap,
  userAgent,
} from "../src/openlibrary.ts";
import { VERSION } from "../src/version.ts";

/**
 * Reads a JSON fixture beside this file.
 *
 * @param name - File name under `fixtures/`.
 * @returns The parsed fixture.
 */
function fixture(name: string): unknown {
  return JSON.parse(readFileSync(new URL(`fixtures/${name}`, import.meta.url), "utf8")) as unknown;
}

/**
 * The query parameters of a planned request.
 *
 * @param url - Planned URL.
 * @returns Its search params.
 */
function query(url: string): URLSearchParams {
  const parsed = new URL(url);
  assert.equal(parsed.origin, "https://openlibrary.org");
  assert.equal(parsed.pathname, "/search.json");
  return parsed.searchParams;
}

test("VERSION matches plugin.toml", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  assert.match(manifest, new RegExp(`^version = "${VERSION.replace(/\./g, "\\.")}"$`, "m"));
});

test("clampLimit defaults, floors and caps", () => {
  assert.equal(clampLimit(undefined), DEFAULT_LIMIT);
  assert.equal(clampLimit(0), DEFAULT_LIMIT);
  assert.equal(clampLimit(-3), DEFAULT_LIMIT);
  assert.equal(clampLimit(Number.NaN), DEFAULT_LIMIT);
  assert.equal(clampLimit(3.9), 3);
  assert.equal(clampLimit(1000), MAX_LIMIT);
});

test("normalizeIsbn accepts ISBN-10 and ISBN-13 shapes only", () => {
  assert.equal(normalizeIsbn("978-0-547-92822-7"), "9780547928227");
  assert.equal(normalizeIsbn(" 054792822x "), "054792822X");
  assert.equal(normalizeIsbn("05479282X2"), undefined);
  assert.equal(normalizeIsbn("12345"), undefined);
  assert.equal(normalizeIsbn("97805479282270"), undefined);
});

test("normalizeOlid takes bare and path forms of the right kind", () => {
  assert.equal(normalizeOlid("OL27482W", "W"), "OL27482W");
  assert.equal(normalizeOlid("/works/ol27482w", "W"), "OL27482W");
  assert.equal(normalizeOlid("/books/OL26456609M", "M"), "OL26456609M");
  assert.equal(normalizeOlid("OL27482W", "M"), undefined);
  assert.equal(normalizeOlid("OL27482W extra", "W"), undefined);
  assert.equal(normalizeOlid("", "W"), undefined);
});

test("planLookup prefers an edition id, then a work id, then an ISBN", () => {
  const isbn = { scheme: "isbn" as const, value: "9780547928227" };
  const work = { scheme: "openLibraryWork" as const, value: "/works/OL27482W" };
  const edition = { scheme: "openLibraryEdition" as const, value: "OL26456609M" };

  const byEdition = planLookup({ identifiers: [isbn, work, edition] });
  assert.ok(byEdition);
  assert.equal(query(byEdition.url).get("q"), "edition_key:OL26456609M");

  const byWork = planLookup({ identifiers: [isbn, work], limit: 2 });
  assert.ok(byWork);
  assert.equal(query(byWork.url).get("q"), 'key:"/works/OL27482W"');
  assert.equal(query(byWork.url).get("limit"), "2");
  assert.equal(byWork.limit, 2);

  const byIsbn = planLookup({ identifiers: [isbn] });
  assert.ok(byIsbn);
  const q = query(byIsbn.url);
  assert.equal(q.get("isbn"), "9780547928227");
  assert.equal(q.get("limit"), String(DEFAULT_LIMIT));
  assert.match(q.get("fields") ?? "", /editions\.isbn/);
  assert.equal(byIsbn.titleFilter, undefined);
});

test("planLookup skips malformed and unserved identifiers", () => {
  assert.equal(
    planLookup({
      identifiers: [
        { scheme: "asin", value: "B0036S4B2G", marketplace: "us" },
        { scheme: "isbn", value: "not an isbn" },
        { scheme: "openLibraryEdition", value: "OL1W" },
      ],
    }),
    undefined,
  );
});

test("planLookup refuses an empty identifier list", () => {
  assert.throws(
    () => planLookup({ identifiers: [] }),
    (err: unknown) => err instanceof ProviderFailure && err.code === "invalid_params",
  );
});

test("planSearch sends title, author and a valid language", () => {
  const plan = planSearch({
    title: "  The Hobbit ",
    author: " Tolkien ",
    language: "ENG",
    limit: 50,
  });
  const q = query(plan.url);
  assert.equal(q.get("title"), "The Hobbit");
  assert.equal(q.get("author"), "Tolkien");
  assert.equal(q.get("lang"), "eng");
  assert.equal(q.get("limit"), String(MAX_LIMIT));
  assert.equal(plan.titleFilter, "the hobbit");

  const bare = query(planSearch({ title: "Dune", author: " ", language: "en-US" }).url);
  assert.equal(bare.has("author"), false);
  assert.equal(bare.has("lang"), false);
});

test("planSearch refuses a blank title", () => {
  assert.throws(
    () => planSearch({ title: "   " }),
    (err: unknown) => err instanceof ProviderFailure && err.code === "invalid_params",
  );
});

test("userAgent includes a plausible contact email only", () => {
  assert.equal(
    userAgent("1.2.3", " ops@example.org "),
    "Bookclerk-OpenLibrary/1.2.3 (ops@example.org; https://bookclerk.app)",
  );
  for (const bad of [
    undefined,
    42,
    "",
    "not-an-email",
    "a@b\r\nX-Evil: 1",
    "a(b)@c.d",
    "a b@c.d",
  ]) {
    assert.equal(userAgent("1.2.3", bad), "Bookclerk-OpenLibrary/1.2.3 (https://bookclerk.app)");
  }
});

test("an ISBN hit describes the matched edition", () => {
  const records = recordsFromSearch(fixture("search-isbn.json"), { url: "", limit: 5 });
  assert.equal(records.length, 1);
  assert.deepEqual(records[0], {
    title: "The Hobbit",
    subtitle: "or, There and Back Again",
    description: "In a hole in the ground there lived a hobbit.",
    url: "https://openlibrary.org/books/OL26456609M",
    coverUrl: "https://covers.openlibrary.org/b/id/14624642-L.jpg",
    publisher: "Mariner Books",
    publishedAt: "1966",
    categories: "Fantasy; Arkenstone; Battle of Five Armies; eagles",
    language: "eng",
    abridgement: "unknown",
    workKind: "book",
    expressionKind: "text",
    mediaKind: "print",
    identifiers: [
      { scheme: "openLibraryWork", value: "OL27482W" },
      { scheme: "openLibraryEdition", value: "OL26456609M" },
      { scheme: "isbn", value: "054792822X" },
      { scheme: "isbn", value: "9780547928227" },
    ],
    credits: [{ name: "J.R.R. Tolkien", role: "author", level: "roleDefault" }],
    seriesEntries: [],
    ratingOverall: 4.2978306,
    ratingCount: 507,
  });
});

test("a title search keeps overlapping titles and describes works", () => {
  const plan = planSearch({ title: "the hobbit" });
  const records = recordsFromSearch(fixture("search-title.json"), plan);
  assert.equal(records.length, 1);
  const [hobbit] = records;
  assert.ok(hobbit);
  assert.equal(hobbit.url, "https://openlibrary.org/works/OL27482W");
  assert.deepEqual(hobbit.identifiers, [{ scheme: "openLibraryWork", value: "OL27482W" }]);
  assert.equal(hobbit.description, "In a hole in the ground there lived a hobbit.");
  assert.equal(hobbit.coverUrl, "https://covers.openlibrary.org/b/id/14627509-L.jpg");
  assert.equal(hobbit.publishedAt, "1937");
  assert.equal(hobbit.language, undefined);
  assert.equal(hobbit.publisher, undefined);
});

test("recordsFromSearch stops at the limit", () => {
  const docs = Array.from({ length: 8 }, (_, i) => ({
    key: `/works/OL${i + 1}W`,
    title: `Book ${i}`,
  }));
  assert.equal(recordsFromSearch({ docs }, { url: "", limit: 3 }).length, 3);
});

test("recordsFromSearch treats a missing docs list as no match", () => {
  assert.deepEqual(recordsFromSearch({ numFound: 0 }, { url: "", limit: 5 }), []);
});

test("recordsFromSearch refuses bodies that are not search results", () => {
  for (const body of [null, [], "docs", 3, { docs: "nope" }]) {
    assert.throws(
      () => recordsFromSearch(body, { url: "", limit: 5 }),
      (err: unknown) => err instanceof ProviderFailure && err.code === "unavailable",
    );
  }
});

test("recordFromDoc drops hits without a title or work key", () => {
  assert.equal(recordFromDoc(null), undefined);
  assert.equal(recordFromDoc([]), undefined);
  assert.equal(recordFromDoc({ title: "No key" }), undefined);
  assert.equal(recordFromDoc({ key: "/books/OL1M", title: "Edition key" }), undefined);
  assert.equal(recordFromDoc({ key: "/works/OL1W", title: "   " }), undefined);
  assert.equal(recordFromDoc({ key: "/works/OL1W", title: 7 }), undefined);
});

test("recordFromDoc ignores fields of the wrong type or range", () => {
  const record = recordFromDoc({
    key: "/works/OL1W",
    title: "Odd",
    subtitle: "Odd",
    author_name: ["A", "", 3, "A", "B"],
    first_sentence: { value: "not text" },
    cover_i: -1,
    first_publish_year: 1e9,
    subject: "Fantasy",
    ratings_average: 9,
    ratings_count: 1.5,
    editions: { docs: [{ key: "/works/OL2W", isbn: ["bad", 9780547928227], publish_date: [] }] },
  });
  assert.ok(record);
  assert.deepEqual(record.identifiers, [{ scheme: "openLibraryWork", value: "OL1W" }]);
  assert.deepEqual(
    record.credits.map((c) => c.name),
    ["A", "B"],
  );
  for (const key of [
    "subtitle",
    "description",
    "coverUrl",
    "publishedAt",
    "categories",
    "ratingOverall",
    "ratingCount",
    "language",
  ] as const) {
    assert.equal(record[key], undefined, key);
  }
});

test("recordFromDoc caps long text and long lists", () => {
  const record = recordFromDoc({
    key: "/works/OL1W",
    title: "x".repeat(MAX_TEXT * 2),
    author_name: Array.from({ length: 50 }, (_, i) => `Author ${i}`),
    subject: Array.from({ length: 50 }, (_, i) => `S${i}`),
    editions: {
      docs: [
        {
          key: "/books/OL1M",
          isbn: Array.from({ length: 50 }, (_, i) => String(9780000000000 + i)),
        },
      ],
    },
  });
  assert.ok(record);
  assert.equal(record.title.length, MAX_TEXT);
  assert.equal(record.credits.length, MAX_AUTHORS);
  assert.equal(record.categories?.split("; ").length, MAX_SUBJECTS);
  assert.equal(record.identifiers.filter((i) => i.scheme === "isbn").length, 10);
});

test("titlesOverlap matches containment either way", () => {
  assert.equal(titlesOverlap("the hobbit", "The Hobbit, or There and Back Again"), true);
  assert.equal(titlesOverlap("the hobbit, or there and back again", "The Hobbit"), true);
  assert.equal(titlesOverlap("the hobbit", "Dune"), false);
});
