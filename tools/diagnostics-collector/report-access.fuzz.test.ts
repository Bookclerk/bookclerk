// The report lookup boundaries (fuzzing ADR decision 6): seed corpus replay
// and seeded mutation against oracles written without report-access.ts's or
// storage.ts's own helpers.
//
// - parseLookupInput reads the body anyone can post to /lookup and
//   /lookup/delete (`corpus/lookup_input/`, each seed's first line is the
//   Content-Type); its result names the report a code is checked against
//   and whether a form confirmed a deletion.
// - decodeVersionPage reads B2's version listing for one object
//   (`corpus/b2_versions/`); its result is every file ID a deletion removes,
//   so nothing of another object may get through.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { decodeVersionPage } from "./dashboard/storage.ts";
import { MAX_LOOKUP_CHARS, parseLookupInput, type LookupInput } from "./report-access.ts";
import { choose, mutate, rng, seeds } from "./test-support/fuzz.ts";

/** The object key the version seeds list. */
const KEY = "diagnostics/0.9.0/3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f.json";

/**
 * A plain object (not null, not an array).
 *
 * @param v - Anything.
 * @returns Whether it is one.
 */
function plain(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Oracle report ID: trimmed, lowercased, then a version-4 UUID by position.
 *
 * @param v - Candidate.
 * @returns The ID, or null.
 */
function oracleId(v: string): string | null {
  const id = v.trim().toLowerCase();
  if (id.length !== 36) return null;
  for (let i = 0; i < 36; i++) {
    const c = id[i] ?? "";
    if (i === 8 || i === 13 || i === 18 || i === 23) {
      if (c !== "-") return null;
    } else if (!"0123456789abcdef".includes(c)) return null;
  }
  return id[14] === "4" && "89ab".includes(id[19] ?? "") ? id : null;
}

/**
 * Oracle access code, by regular expressions.
 *
 * @param v - Candidate.
 * @returns The canonical code, or null.
 */
function oracleCode(v: string): string | null {
  if (v.length > 64) return null;
  const s = v.toUpperCase().replace(/[- ]/g, "").replace(/O/g, "0").replace(/[IL]/g, "1");
  return /^[0-9A-HJKMNP-TV-Z]{16}$/.test(s) ? s : null;
}

/**
 * Oracle form decoding: `+` is a space, `%XX` a byte, other characters
 * their UTF-8 bytes; the bytes read as UTF-8 with replacement.
 *
 * @param s - One name or value as sent.
 * @returns The decoded text.
 */
function formDecode(s: string): string {
  const bytes: number[] = [];
  const text = s.replace(/\+/g, " ");
  for (let i = 0; i < text.length; i++) {
    const hex = text.slice(i + 1, i + 3);
    if (text[i] === "%" && /^[0-9A-Fa-f]{2}$/.test(hex)) {
      bytes.push(parseInt(hex, 16));
      i += 2;
      continue;
    }
    const cp = text.codePointAt(i) ?? 0;
    if (cp > 0xffff) i += 1;
    bytes.push(...new TextEncoder().encode(String.fromCodePoint(cp)));
  }
  return new TextDecoder().decode(new Uint8Array(bytes));
}

/**
 * Oracle for parseLookupInput.
 *
 * @param type - Content type.
 * @param body - Body (any value).
 * @param label - Input name for failure messages.
 */
function checkInput(type: string | null, body: unknown, label: string): void {
  const got = parseLookupInput(type, body);
  const expect = (want: LookupInput | null): void => assert.deepEqual(got, want, label);
  if (typeof body !== "string" || body.length > MAX_LOOKUP_CHARS) return expect(null);
  const media = (type ?? "").split(";")[0]?.trim().toLowerCase();
  let id: unknown;
  let code: unknown;
  let confirm = false;
  if (media === "application/json") {
    let data: unknown;
    try {
      data = JSON.parse(body);
    } catch {
      return expect(null);
    }
    if (!plain(data)) return expect(null);
    if (Object.keys(data).sort().join(",") !== "access_code,report_id") return expect(null);
    id = data["report_id"];
    code = data["access_code"];
  } else if (media === "application/x-www-form-urlencoded") {
    const seen: string[] = [];
    for (const piece of body.split("&")) {
      if (piece === "") continue;
      const at = piece.indexOf("=");
      const name = formDecode(at < 0 ? piece : piece.slice(0, at));
      const value = formDecode(at < 0 ? "" : piece.slice(at + 1));
      if (seen.includes(name)) return expect(null);
      seen.push(name);
      if (name === "report_id") id = value;
      else if (name === "access_code") code = value;
      else if (name === "confirm" && value === "yes") confirm = true;
      else return expect(null);
    }
  } else {
    return expect(null);
  }
  if (typeof id !== "string" || typeof code !== "string") return expect(null);
  const reportId = oracleId(id);
  const accessCode = oracleCode(code);
  if (reportId === null || accessCode === null) return expect(null);
  return expect({ report_id: reportId, access_code: accessCode, confirm });
}

/**
 * Splits a lookup seed into its content type and body.
 *
 * @param raw - Seed text.
 * @returns Content type (first line) and body (the rest).
 */
function splitSeed(raw: string): [string, string] {
  const at = raw.indexOf("\n");
  return at < 0 ? [raw, ""] : [raw.slice(0, at), raw.slice(at + 1)];
}

/**
 * Oracle for decodeVersionPage.
 *
 * @param text - Listing text.
 * @param label - Input name for failure messages.
 */
function checkVersions(text: string, label: string): void {
  let data: unknown = null;
  try {
    data = JSON.parse(text);
  } catch {
    data = null;
  }
  const got = decodeVersionPage(data, KEY);
  const files = plain(data) && Array.isArray(data["files"]) ? data["files"] : [];
  const fileIds: string[] = [];
  for (const f of files) {
    if (plain(f) && f["fileName"] === KEY && typeof f["fileId"] === "string") {
      fileIds.push(f["fileId"]);
    }
  }
  const more =
    plain(data) && data["nextFileName"] === KEY && typeof data["nextFileId"] === "string";
  assert.deepEqual(got, { fileIds, more }, label);
}

const INPUT_FRAGMENTS = [
  "{",
  "}",
  '"',
  ":",
  ",",
  "&",
  "=",
  "+",
  "%",
  "%2D",
  "%2",
  "%E2%82",
  "%00",
  " ",
  "-",
  '"report_id":',
  '"access_code":',
  "report_id=",
  "access_code=",
  "confirm=yes",
  "confirm=",
  "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f",
  "K7QM-3XRD-PW9T-H2CF",
  "O",
  "I",
  "L",
  "U",
  "ß",
  "ı",
  "4",
  " ",
  "x".repeat(70),
];

const VERSION_FRAGMENTS = [
  "{",
  "}",
  "[",
  "]",
  ",",
  '"',
  "null",
  "7",
  `"${KEY}"`,
  '"fileName":',
  '"fileId":',
  '"nextFileName":',
  '"nextFileId":',
  '"4_z"',
  '"action":"hide"',
  '"__proto__":{}',
  ".bak",
];

const TYPES = [
  "application/json",
  "application/x-www-form-urlencoded",
  "Application/JSON; charset=utf-8",
  "text/plain",
  "",
];

describe("parseLookupInput", () => {
  it("replays the seed corpus", () => {
    for (const [name, raw] of seeds("lookup_input")) {
      const [type, body] = splitSeed(raw);
      checkInput(type, body, name);
    }
    const parse = (name: string): LookupInput | null =>
      parseLookupInput(...splitSeed(seeds("lookup_input").find(([n]) => n === name)?.[1] ?? ""));
    for (const ok of ["json.txt", "json_loose.txt", "form.txt", "form_percent.txt"]) {
      assert.equal(parse(ok)?.access_code, "K7QM3XRDPW9TH2CF", ok);
      assert.equal(parse(ok)?.report_id, "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f", ok);
    }
    assert.deepEqual(parse("form_confirm.txt"), {
      report_id: "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f",
      access_code: "01103XRDPW9TH2CF",
      confirm: true,
    });
    for (const off of [
      "json_extra_field.txt",
      "json_v1_id.txt",
      "json_bad_letter.txt",
      "json_array.txt",
      "form_duplicate.txt",
      "form_confirm_no.txt",
      "form_extra_field.txt",
      "form_bad_percent.txt",
      "other_type.txt",
      "empty.txt",
    ]) {
      assert.equal(parse(off), null, off);
    }
    for (const v of [null, undefined, 7, {}, "x".repeat(MAX_LOOKUP_CHARS + 1)]) {
      checkInput("application/json", v, String(v).slice(0, 20));
    }
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("lookup_input").map(([, raw]) => splitSeed(raw)[1]);
    const rand = rng(20261119);
    for (let n = 0; n < 6000; n++) {
      const type = choose(rand, TYPES);
      const input = mutate(rand, choose(rand, all), INPUT_FRAGMENTS);
      checkInput(type, input, `iteration ${n} (${type}): ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});

describe("decodeVersionPage", () => {
  it("replays the seed corpus", () => {
    for (const [name, raw] of seeds("b2_versions")) checkVersions(raw, name);
    const page = (name: string) =>
      decodeVersionPage(
        JSON.parse(seeds("b2_versions").find(([n]) => n === name)?.[1] ?? "null"),
        KEY,
      );
    assert.deepEqual(page("one.json"), { fileIds: ["4_z1"], more: false });
    assert.deepEqual(page("versions_more.json"), { fileIds: ["4_z2", "4_z1"], more: true });
    assert.deepEqual(page("odd.json"), { fileIds: [], more: false });
  });

  it("holds the oracle under seeded mutation", () => {
    const all = seeds("b2_versions").map(([, raw]) => raw);
    const rand = rng(20261120);
    for (let n = 0; n < 4000; n++) {
      const input = mutate(rand, choose(rand, all), VERSION_FRAGMENTS);
      checkVersions(input, `iteration ${n}: ${JSON.stringify(input).slice(0, 300)}`);
    }
  });
});
