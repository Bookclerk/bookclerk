/**
 * Unit tests for each dialect, against the seed files in `fuzz/` and small
 * inline files.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, test } from "node:test";

import type { MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";
import { htmlToText } from "../src/libation.ts";
import { DIALECTS, parseFile, renderFile, type Dialect } from "../src/dialects.ts";
import {
  DialectFailure,
  MAX_DESCRIPTION_BYTES,
  MAX_FILE_BYTES,
  MAX_LIST_ENTRIES,
  MAX_TEXT_BYTES,
  RecordBuilder,
  cutBytes,
  decodeText,
  encodeFile,
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

/** A stand-in writable dialect, for the routing that no shipped dialect uses yet. */
const ECHO: ReadonlyMap<string, Dialect> = new Map<string, Dialect>([
  [
    "echo",
    {
      parse: () => record(),
      render: (_existing, r, fields) => encodeFile(`${r.title ?? ""} ${fields.join(",")}`),
    },
  ],
]);

describe("requests", () => {
  test("refuse an unknown dialect", () => {
    refuses(() => parseFile("opf", utf8("")), /unknown dialect `opf`/);
    refuses(() => renderFile("__proto__", utf8(""), record(), []), /unknown dialect/);
  });

  test("refuse rendering a read-only dialect", () => {
    refuses(() => renderFile("libation_json", utf8("{}"), record(), ["title"]), /read-only/);
  });

  test("refuse files over the cap", () => {
    const big = new Uint8Array(MAX_FILE_BYTES + 1);
    refuses(() => parseFile("libation_json", big), /over the 262144 byte cap/);
    refuses(() => renderFile("echo", big, record(), [], ECHO), /existing file is over/);
    refuses(() => encodeFile("x".repeat(MAX_FILE_BYTES + 1)), /rendered file is 262145 bytes/);
    assert.equal(encodeFile("é").length, 2);
  });

  test("route a render to the dialect's writer", () => {
    const out = renderFile("echo", utf8("old"), record({ title: "T" }), ["title"], ECHO);
    assert.equal(str(out), "T title");
    assert.deepEqual(parseFile("echo", utf8("x"), ECHO).present, []);
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

describe("libation_json", () => {
  test("reads the store's catalog record", () => {
    const parsed = parseFile("libation_json", seed("libation_json/seed_00.json"));
    assert.equal(parsed.title, "Dune");
    assert.equal(parsed.publisher, "Macmillan Audio");
    assert.equal(parsed.publishedAt, "2007-01-01");
    assert.equal(parsed.durationMs, 75780000);
    assert.deepEqual(parsed.genres, [
      "Science Fiction & Fantasy",
      "Science Fiction",
      "Space Opera",
    ]);
    assert.deepEqual(parsed.seriesEntries, [
      { name: "Dune", position: "1", storeSeriesId: "B00GHWF2GM" },
    ]);
    assert.deepEqual(
      parsed.chapters.map((c) => [c.title, c.depth]),
      [
        ["Opening Credits", 0],
        ["Book One: Dune", 0],
        ["Chapter 1", 1],
        ["Chapter 2", 1],
        ["End Credits", 0],
      ],
    );
    assert.match(parsed.description ?? "", /^Set on the desert planet Arrakis, Dune is/);
  });

  test("reads a wrapped product and the runtime in minutes", () => {
    const parsed = parseFile("libation_json", seed("libation_json/seed_01.json"));
    assert.equal(parsed.durationMs, 61 * 60_000);
    assert.equal(parsed.abridgement, "abridged");
    assert.equal(parsed.description, "Plain text\nnext");
    assert.deepEqual(parsed.seriesEntries, []);
    assert.ok(parsed.present.includes("series"));
  });

  test("reads an unknown format as stated without a value", () => {
    const parsed = parseFile("libation_json", utf8('{"format_type":"original_recording"}'));
    assert.deepEqual(parsed.present, ["abridgement"]);
    assert.equal(parsed.abridgement, "unknown");
  });

  test("refuses a file that is not a JSON object", () => {
    refuses(() => parseFile("libation_json", utf8("[1]")), /not a JSON object/);
    refuses(() => parseFile("libation_json", utf8("{")), /not valid JSON/);
  });

  test("turns summary HTML into text", () => {
    assert.equal(htmlToText("a<br>b<BR />c</p>d"), "a\nb\nc\n\nd");
    assert.equal(htmlToText("&#65;&#x42;&#0;&#xD800;&amp;&unknown;"), "AB��&&unknown;");
    assert.equal(htmlToText("x  \n\n\n\ny"), "x\n\ny");
  });
});

describe("credit roles", () => {
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
});
