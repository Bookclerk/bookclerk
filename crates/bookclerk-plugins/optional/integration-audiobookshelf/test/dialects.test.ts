/**
 * Unit tests for each dialect, against the seed files in `fuzz/` and small
 * inline files.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, test } from "node:test";

import type { MetadataField, MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";
import { canonicalNumber } from "../src/abs-json.ts";
import { DIALECTS, parseFile, renderFile } from "../src/dialects.ts";
import { splitNames } from "../src/plain-text.ts";
import {
  DialectFailure,
  MAX_DESCRIPTION_BYTES,
  MAX_FILE_BYTES,
  MAX_LIST_ENTRIES,
  MAX_TEXT_BYTES,
  RecordBuilder,
  cutBytes,
  decodeText,
  isPartialDate,
  labels,
  line,
  longText,
  orderChapters,
  partialDate,
  secondsToMs,
  wholeMs,
} from "../src/text.ts";

/** Encodes text as UTF-8. */
const utf8 = (text: string): Uint8Array => new TextEncoder().encode(text);

/** Decodes UTF-8 bytes. */
const str = (bytes: Uint8Array): string => new TextDecoder().decode(bytes);

/**
 * Reads one seed file.
 *
 * @param path - Path under `fuzz/`.
 * @returns Its bytes.
 */
function seed(path: string): Uint8Array {
  return readFileSync(new URL(`../fuzz/${path}`, import.meta.url));
}

/**
 * A record with nothing in it, to fill for render requests.
 *
 * @param fill - Fields to set.
 * @returns The record.
 */
function record(fill: Partial<MetadataFileRecord> = {}): MetadataFileRecord {
  return { ...new RecordBuilder().build(), ...fill };
}

/**
 * Asserts that a call fails with `invalid_params`.
 *
 * @param work - The call.
 * @param message - Pattern the message must match.
 */
function refuses(work: () => unknown, message: RegExp): void {
  assert.throws(work, (err: unknown) => {
    assert.ok(err instanceof DialectFailure);
    assert.equal(err.code, "invalid_params");
    assert.match(err.message, message);
    return true;
  });
}

describe("manifest", () => {
  /** `[[metadata_files.dialects]]` rows of `plugin.toml`: id and `writes`. */
  const rows = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8")
    .split("[[metadata_files.dialects]]")
    .slice(1)
    .map((row) => ({
      id: /^id = "([^"]+)"$/m.exec(row)?.[1],
      writes: /^writes = true$/m.test(row),
    }));

  test("declares exactly the dialects the code serves", () => {
    assert.deepEqual(rows.map((r) => r.id).sort(), [...DIALECTS.keys()].sort());
  });

  test("marks exactly the dialects with a renderer as writable", () => {
    for (const row of rows) {
      assert.equal(DIALECTS.get(row.id ?? "")?.render !== undefined, row.writes, `${row.id}`);
    }
  });
});

describe("requests", () => {
  test("refuse an unknown dialect", () => {
    refuses(() => parseFile("opf", utf8("")), /unknown dialect `opf`/);
    refuses(() => renderFile("__proto__", utf8(""), record(), []), /unknown dialect/);
  });

  test("refuse files over the cap", () => {
    const big = new Uint8Array(MAX_FILE_BYTES + 1);
    refuses(() => parseFile("desc_txt", big), /over the 262144 byte cap/);
    refuses(() => renderFile("desc_txt", big, record(), []), /existing file is over/);
    const description = "x".repeat(MAX_FILE_BYTES);
    refuses(
      () => renderFile("desc_txt", utf8(""), record({ description }), ["description"]),
      /rendered file is 262145 bytes/,
    );
  });
});

describe("cleaning", () => {
  test("decodes UTF-8 and falls back to Windows-1252", () => {
    assert.equal(decodeText(utf8("﻿Café")), "Café");
    assert.equal(decodeText(new Uint8Array([0x93, 0x41, 0x94, 0x81, 0xe9])), "“A”�é");
  });

  test("keeps labels on one line within the cap", () => {
    assert.equal(line("  a\r\nb\u0000c\u0085 "), "a b c");
    assert.equal(line("\uD800x"), "�x");
    assert.equal(line(" \t "), undefined);
    assert.equal(line(5), undefined);
    const long = line("é".repeat(MAX_TEXT_BYTES));
    assert.equal(utf8(long ?? "").length, MAX_TEXT_BYTES);
    assert.equal(cutBytes("a😀", 4), "a");
    assert.equal(cutBytes("a😀", 5), "a😀");
  });

  test("keeps line breaks in a description", () => {
    assert.equal(longText(" a\r\nb\rc\td\u0007 "), "a\nb\nc\td");
    assert.equal(longText(""), undefined);
    assert.equal(longText(null), undefined);
    assert.equal(
      utf8(longText("é".repeat(MAX_DESCRIPTION_BYTES)) ?? "").length,
      MAX_DESCRIPTION_BYTES,
    );
  });

  test("dedupes and caps label lists", () => {
    assert.deepEqual(labels(["a", " a ", "", 3, "b"]), ["a", "b"]);
    assert.deepEqual(labels("a"), []);
    const many = Array.from({ length: MAX_LIST_ENTRIES + 5 }, (_, i) => `g${i}`);
    assert.equal(labels(many).length, MAX_LIST_ENTRIES);
  });

  test("reads dates people write", () => {
    assert.equal(partialDate("2010-08-31"), "2010-08-31");
    assert.equal(partialDate("2010-08-31T00:00:00Z"), "2010-08-31");
    assert.equal(partialDate("2010-08"), "2010-08");
    assert.equal(partialDate("09-20-2020"), "2020-09-20");
    assert.equal(partialDate("9/2/2020"), "2020-09-02");
    assert.equal(partialDate("©1937, renewed 1966"), "1966");
    assert.equal(partialDate("2010-02-30"), "2010");
    assert.equal(partialDate("13-01-2020"), "2020");
    assert.equal(partialDate("no year"), undefined);
    assert.equal(partialDate(2010), undefined);
    assert.ok(isPartialDate("2024-02-29"));
    assert.ok(!isPartialDate("2023-02-29"));
    assert.ok(!isPartialDate("1900-02-29"));
    assert.ok(isPartialDate("2000-02-29"));
    assert.ok(!isPartialDate("2000-00"));
  });

  test("converts offsets", () => {
    assert.equal(secondsToMs(1.2345), 1235);
    assert.equal(secondsToMs(-1), undefined);
    assert.equal(secondsToMs(1e300), undefined);
    assert.equal(secondsToMs("1"), undefined);
    assert.equal(wholeMs(10.4), 10);
    assert.equal(wholeMs(Number.NaN), undefined);
  });

  test("orders chapters and their depths", () => {
    const chapters = orderChapters([
      { title: "b", startMs: 10, depth: 3 },
      { title: "a", startMs: 0, depth: 2 },
      { title: "c", startMs: 10, depth: 5 },
    ]);
    assert.deepEqual(
      chapters.map((c) => [c.title, c.depth]),
      [
        ["a", 0],
        ["b", 1],
        ["c", 2],
      ],
    );
  });
});

describe("abs_metadata_json", () => {
  test("reads a file Audiobookshelf wrote", () => {
    const parsed = parseFile("abs_metadata_json", seed("abs_metadata_json/seed_00.json"));
    assert.equal(parsed.title, "The Way of Kings");
    assert.equal(parsed.subtitle, "Book One of the Stormlight Archive");
    assert.equal(parsed.publishedAt, "2010-08-31");
    assert.equal(parsed.abridgement, "unabridged");
    assert.deepEqual(parsed.seriesEntries, [
      { name: "The Stormlight Archive", position: "1" },
      { name: "Cosmere", position: "2.5" },
    ]);
    assert.deepEqual(
      parsed.credits.map((c) => `${c.role}:${c.name}`),
      ["author:Brandon Sanderson", "narrator:Michael Kramer", "narrator:Kate Reading"],
    );
    assert.deepEqual(parsed.identifiers, [
      { scheme: "isbn", value: "9781427209221" },
      { scheme: "asin", value: "B003P2WO5E" },
    ]);
    assert.deepEqual(parsed.chapters[1], {
      title: "Chapter 1: The Boy Who Lived",
      startMs: 1210500,
      lengthMs: 1810750,
      depth: 0,
    });
  });

  test("lets a legacy nested value win, as Audiobookshelf does", () => {
    const parsed = parseFile("abs_metadata_json", seed("abs_metadata_json/seed_01.json"));
    assert.equal(parsed.title, "Legacy Title");
    assert.equal(parsed.abridgement, "abridged");
    assert.equal(parsed.publishedAt, "1999");
  });

  test("states null and empty keys without values", () => {
    const parsed = parseFile("abs_metadata_json", seed("abs_metadata_json/seed_02.json"));
    assert.ok(parsed.present.includes("subtitle"));
    assert.equal(parsed.subtitle, undefined);
    assert.deepEqual(parsed.creditRoles, ["author", "narrator"]);
    assert.deepEqual(parsed.credits, []);
    assert.ok(!parsed.present.includes("chapters"));
  });

  test("refuses a file that is not a JSON object", () => {
    refuses(() => parseFile("abs_metadata_json", utf8("[1]")), /not a JSON object/);
    refuses(() => parseFile("abs_metadata_json", utf8("{")), /not valid JSON/);
    refuses(
      () => renderFile("abs_metadata_json", utf8("null"), record(), ["title"]),
      /not a JSON object/,
    );
  });

  test("renders into the existing file and keeps unknown keys", () => {
    const fields: MetadataField[] = [
      "title",
      "subtitle",
      "publishedAt",
      "credits",
      "series",
      "abridgement",
      "identifiers",
      "genres",
      "chapters",
    ];
    const out = renderFile(
      "abs_metadata_json",
      seed("abs_metadata_json/seed_01.json"),
      record({
        title: "New",
        publishedAt: "2001-02",
        credits: [{ name: "N", role: "narrator", level: "roleDefault" }],
        creditRoles: ["narrator"],
        seriesEntries: [{ name: "S", position: "3" }, { name: "T" }],
        abridgement: "unabridged",
        identifiers: [
          { scheme: "openLibraryEdition", value: "OL1M" },
          { scheme: "asin", value: "B1" },
        ],
        chapters: [
          { title: "One", startMs: 0, depth: 0 },
          { title: "Two", startMs: 1500, lengthMs: 500, depth: 0 },
        ],
      }),
      fields,
    );
    const file = JSON.parse(str(out)) as Record<string, unknown>;
    assert.equal(file["title"], "New");
    assert.ok(!("subtitle" in file));
    assert.equal(file["publishedYear"], "2001");
    assert.equal(file["publishedDate"], "2001-02");
    assert.deepEqual(file["narrators"], ["N"]);
    assert.deepEqual(file["series"], ["S #3", "T"]);
    assert.equal(file["abridged"], false);
    assert.equal(file["asin"], "B1");
    assert.ok(!("isbn" in file));
    assert.ok(!("genres" in file));
    assert.deepEqual(file["chapters"], [
      { id: 0, start: 0, end: 1.5, title: "One" },
      { id: 1, start: 1.5, end: 2, title: "Two" },
    ]);
    assert.deepEqual(file["customKey"], { kept: [1, 2, 3] });
    // The nested values the new ones replace are gone; the author role was
    // not stated, so the nested authors stay.
    assert.deepEqual(file["metadata"], { authors: ["Old Author"] });
    assert.ok(str(out).endsWith("}\n"));
    assert.ok(str(out).startsWith('{\n  "tags"'));

    const reread = parseFile("abs_metadata_json", out);
    assert.equal(reread.title, "New");
    assert.equal(reread.publishedAt, "2001-02");
    assert.equal(reread.abridgement, "unabridged");
  });

  test("creates a file and removes values", () => {
    const created = renderFile(
      "abs_metadata_json",
      new Uint8Array(),
      record({ title: "T", publishedAt: "1999", abridgement: "abridged" }),
      ["title", "publishedAt", "abridgement", "tags"],
    );
    assert.deepEqual(JSON.parse(str(created)), {
      title: "T",
      publishedYear: "1999",
      abridged: true,
    });
    const removed = renderFile("abs_metadata_json", created, record(), [
      "title",
      "publishedAt",
      "abridgement",
    ]);
    assert.equal(str(removed), "{}\n");
    // Fields the format does not carry change nothing.
    const untouched = renderFile("abs_metadata_json", created, record({ durationMs: 5 }), [
      "duration",
      "expressionKind",
    ]);
    assert.deepEqual(JSON.parse(str(untouched)), JSON.parse(str(created)));
  });
});

describe("abs_nfo", () => {
  test("reads a release NFO", () => {
    const parsed = parseFile("abs_nfo", seed("abs_nfo/seed_00.nfo"));
    assert.equal(parsed.title, "The Hobbit");
    assert.equal(parsed.subtitle, "Or There and Back Again");
    assert.equal(parsed.publishedAt, "2020-09-20");
    assert.deepEqual(parsed.seriesEntries, [{ name: "Middle-earth", position: "0" }]);
    assert.deepEqual(parsed.genres, ["Fantasy", "Classics"]);
    assert.equal(parsed.abridgement, "unabridged");
    assert.equal(
      parsed.description,
      "In a hole in the ground there lived a hobbit.\n\nNot a nasty, dirty, wet hole.",
    );
  });

  test("reads copyright years and Windows line endings", () => {
    const parsed = parseFile("abs_nfo", seed("abs_nfo/seed_01.nfo"));
    assert.equal(parsed.title, "Alone");
    assert.ok(parsed.present.includes("subtitle"));
    assert.equal(parsed.publishedAt, "2001");
    assert.equal(parsed.abridgement, "unabridged");
    assert.deepEqual(parsed.identifiers, [{ scheme: "isbn", value: "9780000000002" }]);
    assert.deepEqual(parsed.tags, ["one", "two"]);
  });

  test("reads the abridgement answers Audiobookshelf reads", () => {
    const abridged = (text: string): string => parseFile("abs_nfo", utf8(text)).abridgement;
    assert.equal(abridged("Unabridged: No"), "abridged");
    assert.equal(abridged("Abridged: Yes"), "abridged");
    assert.equal(abridged("Abridged: no"), "unabridged");
    assert.equal(abridged("Unabridged:"), "unknown");
  });

  test("states nothing for a file without keys or an empty value", () => {
    assert.deepEqual(parseFile("abs_nfo", seed("abs_nfo/seed_02.nfo")).present, []);
    assert.deepEqual(parseFile("abs_nfo", utf8("Author:\nBook Description\n===\n")).present, []);
  });

  test("edits lines in place and keeps everything else", () => {
    const out = renderFile(
      "abs_nfo",
      seed("abs_nfo/seed_00.nfo"),
      record({
        subtitle: "There and Back",
        publisher: "New House",
        credits: [{ name: "A", role: "author", level: "roleDefault" }],
        creditRoles: ["author"],
        tags: ["t1", "t2"],
        description: "Short.",
      }),
      ["subtitle", "publisher", "language", "credits", "tags", "description", "publishedAt"],
    );
    const text = str(out);
    assert.match(text, /^ Title: {18}The Hobbit: There and Back$/m);
    assert.match(text, /^ Publisher: {14}New House$/m);
    assert.match(text, /^ Author: {17}A$/m);
    // The narrator role was not stated, so its line stays.
    assert.match(text, /^ Read By: {16}Andy Serkis$/m);
    assert.doesNotMatch(text, /Language/);
    assert.doesNotMatch(text, /Copyright|Audible\.com Release/);
    assert.match(text, / Bitrate: {16}64 kbps\nTags: t1, t2\n\nBook Description\n=+\nShort\.\n$/);
  });

  test("creates a file, keeps line endings and removes sections", () => {
    const created = renderFile(
      "abs_nfo",
      new Uint8Array(),
      record({ title: "T", abridgement: "abridged", description: "D" }),
      ["title", "abridgement", "description"],
    );
    assert.equal(
      str(created),
      "Title: T\nUnabridged: No\n\nBook Description\n================\nD\n",
    );
    const crlf = renderFile(
      "abs_nfo",
      seed("abs_nfo/seed_01.nfo"),
      record({ seriesEntries: [{ name: "S" }], abridgement: "unabridged" }),
      ["series", "abridgement", "title", "subtitle"],
    );
    assert.equal(
      str(crlf),
      "Narrator: Jane Doe, John Roe\r\nBook Copyright: (c) 2001 Someone\r\nUnabridged: Yes\r\n" +
        "ISBN-13: 9780000000002\r\nTags: one,two, ,two\r\nSeries Name: S\r\n",
    );
    const emptied = renderFile("abs_nfo", created, record(), [
      "title",
      "abridgement",
      "description",
    ]);
    assert.equal(emptied.length, 0);
  });

  test("puts new keys after the last key line, or after the text", () => {
    const out = renderFile("abs_nfo", utf8("Intro text\n\n"), record({ language: "en" }), [
      "language",
    ]);
    assert.equal(str(out), "Intro text\nLanguage: en\n\n");
  });
});

describe("desc_txt and reader_txt", () => {
  test("read and write a description", () => {
    const parsed = parseFile("desc_txt", seed("desc_txt/seed_00.txt"));
    assert.equal(parsed.description, "A boy discovers he is a wizard.\n\nHe goes to school.");
    assert.deepEqual(parseFile("desc_txt", seed("desc_txt/seed_01.txt")).present, ["description"]);
    const out = renderFile("desc_txt", utf8("old"), record({ description: "new" }), [
      "description",
    ]);
    assert.equal(str(out), "new\n");
    assert.equal(renderFile("desc_txt", utf8("old"), record(), ["description"]).length, 0);
    assert.equal(str(renderFile("desc_txt", utf8("old"), record(), [])), "old");
  });

  test("read narrators from the first line", () => {
    assert.deepEqual(splitNames("A & B; C and D,E"), ["A", "B", "C", "D", "E"]);
    assert.deepEqual(splitNames("Sandy Anderson"), ["Sandy Anderson"]);
    const parsed = parseFile("reader_txt", seed("reader_txt/seed_00.txt"));
    assert.deepEqual(
      parsed.credits.map((c) => c.name),
      ["Jim Dale", "Stephen Fry"],
    );
    const latin1 = parseFile("reader_txt", seed("reader_txt/seed_02.txt"));
    assert.equal(latin1.credits[0]?.name, "étienne Café");
    assert.deepEqual(parseFile("reader_txt", utf8("")).creditRoles, ["narrator"]);
  });

  test("write narrators over the first line only", () => {
    const narrators = record({
      credits: [
        { name: "X", role: "narrator", level: "roleDefault" },
        { name: "Y", role: "author", level: "roleDefault" },
        { name: "Z", role: "narrator", level: "roleDefault" },
      ],
      creditRoles: ["narrator", "author"],
    });
    const out = renderFile("reader_txt", seed("reader_txt/seed_00.txt"), narrators, ["credits"]);
    assert.equal(str(out), "X, Z\nsecond line kept\n");
    assert.equal(str(renderFile("reader_txt", utf8(""), narrators, ["credits"])), "X, Z\n");
    const nobody = record({ creditRoles: ["narrator"] });
    assert.equal(renderFile("reader_txt", utf8("A\n"), nobody, ["credits"]).length, 0);
    assert.equal(str(renderFile("reader_txt", utf8("A\nB"), nobody, ["credits"])), "\nB");
    const unknown = record({ creditRoles: ["author"] });
    assert.equal(str(renderFile("reader_txt", utf8("A\n"), unknown, ["credits"])), "A\n");
  });
});

describe("values a format cannot hold", () => {
  test("a credit role cut off by the list cap stays unknown", () => {
    const authors = Array.from({ length: MAX_LIST_ENTRIES }, (_, i) => `A${i}`);
    const parsed = new RecordBuilder()
      .credits("author", authors)
      .credits("narrator", ["N"])
      .build();
    assert.equal(parsed.credits.length, MAX_LIST_ENTRIES);
    assert.deepEqual(parsed.creditRoles, ["author"]);
    const again = new RecordBuilder().credits("author", ["A"]).credits("author", ["A"]).build();
    assert.deepEqual(again.credits.length, 1);
    const empty = new RecordBuilder().credits("narrator", []).build();
    assert.deepEqual(empty.creditRoles, ["narrator"]);
    assert.deepEqual(empty.present, ["credits"]);
  });

  test("metadata.json refuses series that would not read back", () => {
    const write = (name: string, position?: string): Uint8Array =>
      renderFile(
        "abs_metadata_json",
        new Uint8Array(),
        record({ seriesEntries: [{ name, ...(position === undefined ? {} : { position }) }] }),
        ["series"],
      );
    assert.deepEqual(JSON.parse(str(write("S", "2.5")))["series"], ["S #2.5"]);
    assert.deepEqual(JSON.parse(str(write("S #1", "2")))["series"], ["S #1 #2"]);
    refuses(() => write("S", "Part 2"), /cannot be written to metadata.json/);
    refuses(() => write("S", "#2"), /cannot be written/);
    refuses(() => write("Book #1"), /cannot be written/);
  });

  test("metadata.json refuses a number a re-write would change", () => {
    const render = (text: string): Uint8Array =>
      renderFile("abs_metadata_json", utf8(text), record({ title: "T" }), ["title"]);
    refuses(() => render('{"id": 9007199254740993}'), /9007199254740993, which cannot be kept/);
    refuses(() => render('{"x": [1e400]}'), /cannot be kept exactly/);
    // Spellings a double rounds: exponent and fraction forms past 2^53, an
    // underflow, and more digits than a double keeps.
    refuses(() => render('{"x": 9007199254740993e0}'), /9007199254740993e0, which cannot/);
    refuses(() => render('{"x": 9007199254740993.0}'), /cannot be kept exactly/);
    refuses(() => render('{"x": 1e-400}'), /1e-400, which cannot/);
    refuses(() => render('{"x": 0.10000000000000000001}'), /cannot be kept exactly/);
    const spelled = render('{"a": 1.0, "b": 1E2, "c": -0, "d": 0.5e-1, "e": 1e21}');
    assert.deepEqual(JSON.parse(str(spelled)), {
      a: 1,
      b: 100,
      c: 0,
      d: 0.05,
      e: 1e21,
      title: "T",
    });
    const kept = render(
      '{"n": -12, "f": 1.5e3, "s": "9007199254740993 \\" 1e400", "big": 9007199254740991}',
    );
    assert.deepEqual(JSON.parse(str(kept)), {
      n: -12,
      f: 1500,
      s: '9007199254740993 " 1e400',
      big: 9007199254740991,
      title: "T",
    });
  });

  test("number spellings compare by exact value", () => {
    assert.equal(canonicalNumber("1500"), canonicalNumber("1.5e3"));
    assert.equal(canonicalNumber("0.050"), "5e-2");
    assert.equal(canonicalNumber("-0.0e5"), "0");
    assert.equal(canonicalNumber("1e+21"), "1e21");
    assert.equal(canonicalNumber("12e99999999999999999999"), "12e99999999999999999999");
    assert.equal(canonicalNumber("not"), "not");
  });

  test("a compact file can render past the size cap", () => {
    const existing = utf8(`{"x":"${"a".repeat(MAX_FILE_BYTES - 16)}","y":[1]}`);
    assert.ok(existing.length <= MAX_FILE_BYTES);
    assert.equal(parseFile("abs_metadata_json", existing).present.length, 0);
    refuses(() => renderFile("abs_metadata_json", existing, record(), []), /rendered file is/);
  });

  test("an NFO refuses list entries with commas and titles with colons", () => {
    const authors = record({
      credits: [{ name: "King, Jr.", role: "author", level: "roleDefault" }],
      creditRoles: ["author"],
    });
    refuses(() => renderFile("abs_nfo", new Uint8Array(), authors, ["credits"]), /holds a comma/);
    refuses(
      () => renderFile("abs_nfo", new Uint8Array(), record({ genres: ["a,b"] }), ["genres"]),
      /holds a comma/,
    );
    refuses(
      () => renderFile("abs_nfo", new Uint8Array(), record({ title: "A: B" }), ["title"]),
      /holds a colon/,
    );
    // A kept title is the file's own line, colon and all.
    const out = renderFile("abs_nfo", utf8("Title: A: B\n"), record({ subtitle: "C" }), [
      "subtitle",
    ]);
    assert.equal(str(out), "Title: A: C\n");
  });

  test("an NFO keeps lone CR line endings", () => {
    const out = renderFile(
      "abs_nfo",
      utf8("Title: Old\rCustom: Keep\r"),
      record({ title: "New" }),
      ["title"],
    );
    assert.equal(str(out), "Title: New\rCustom: Keep\r");
  });

  test("an NFO keeps a subtitle without a title", () => {
    const cleared = renderFile("abs_nfo", utf8("Title: Old: Keep\n"), record(), ["title"]);
    assert.equal(str(cleared), "Title: : Keep\n");
    assert.equal(parseFile("abs_nfo", cleared).title, undefined);
    assert.equal(parseFile("abs_nfo", cleared).subtitle, "Keep");
    const created = renderFile("abs_nfo", new Uint8Array(), record({ subtitle: "Only" }), [
      "subtitle",
    ]);
    assert.equal(str(created), "Title: : Only\n");
    assert.equal(parseFile("abs_nfo", created).subtitle, "Only");
  });

  test("an NFO refuses description lines it would read as an underline", () => {
    refuses(
      () =>
        renderFile("abs_nfo", new Uint8Array(), record({ description: "First\n===\nLast" }), [
          "description",
        ]),
      /section underline/,
    );
  });

  test("text dialects refuse to rewrite bytes they cannot decode", () => {
    // 0x81 is unused in Windows-1252, so it would come back as U+FFFD.
    const odd = new Uint8Array([...utf8("Title: A\nNote: "), 0x81, 0x0a]);
    assert.equal(parseFile("abs_nfo", odd).title, "A");
    refuses(() => renderFile("abs_nfo", odd, record({ title: "B" }), ["title"]), /lose that byte/);
    const narrators = record({
      credits: [{ name: "X", role: "narrator", level: "roleDefault" }],
      creditRoles: ["narrator"],
    });
    const reader = new Uint8Array([...utf8("A\n"), 0x8d, 0x0a]);
    refuses(() => renderFile("reader_txt", reader, narrators, ["credits"]), /lose that byte/);
    const json = new Uint8Array([...utf8('{"note": "'), 0xff, ...utf8('"}')]);
    assert.equal(parseFile("abs_metadata_json", json).title, undefined);
    refuses(
      () => renderFile("abs_metadata_json", json, record({ title: "T" }), ["title"]),
      /not valid UTF-8/,
    );
  });

  test("reader.txt splits a long space run without a separator quickly", () => {
    const line = `A${" ".repeat(200_000)}B`;
    const started = performance.now();
    const parsed = parseFile("reader_txt", utf8(line));
    assert.ok(performance.now() - started < 1000, "split took over a second");
    assert.equal(parsed.credits.length, 1);
    assert.deepEqual(splitNames(" A , B and C "), [" A", "B", "C "]);
  });

  test("reader.txt refuses names it would split", () => {
    for (const name of ["Doe, Jane", "A & B", "Tom and Jerry", "X;Y", "and"]) {
      const narrators = record({
        credits: [{ name, role: "narrator", level: "roleDefault" }],
        creditRoles: ["narrator"],
      });
      refuses(
        () => renderFile("reader_txt", utf8(""), narrators, ["credits"]),
        /holds a separator/,
      );
    }
    const broken = record({
      credits: [{ name: "Alice\nBob", role: "narrator", level: "roleDefault" }],
      creditRoles: ["narrator"],
    });
    refuses(() => renderFile("reader_txt", utf8(""), broken, ["credits"]), /line break/);
  });
});
