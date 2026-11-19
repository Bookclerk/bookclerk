/**
 * Unit tests for the entrypoint glue, with the SDK's isolate module replaced
 * by stand-ins (the real one imports `cloudflare:workers`, which only exists
 * inside workerd).
 */

import assert from "node:assert/strict";
import { mock, test } from "node:test";

import type { MetadataFileRecord } from "@bookclerk/plugin-sdk/workerd";

/** Stand-in for the SDK's `PluginError`. */
class PluginError extends Error {
  /** Wire code. */
  readonly code: string;

  /**
   * Creates an error.
   *
   * @param code - Wire code.
   * @param message - Message.
   */
  constructor(code: string, message: string) {
    super(message);
    this.code = code;
  }

  /**
   * Builds an error from its wire form, as the SDK does.
   *
   * @param code - Wire code.
   * @param message - Message.
   * @returns The error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}

/** Stand-in for the SDK's entrypoint base classes. */
class Entrypoint {
  /** Bindings. */
  readonly env: unknown;

  /**
   * Keeps the bindings, as `WorkerEntrypoint` does.
   *
   * @param _ctx - Execution context (unused).
   * @param env - Bindings.
   */
  constructor(_ctx: unknown, env: unknown) {
    this.env = env;
  }
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  namedExports: {
    BookclerkEntrypoint: Entrypoint,
    MetadataFilesEntrypoint: Entrypoint,
    PluginError,
  },
});
const { MetadataFiles, default: Plugin } = await import("../src/index.ts");

const files = new MetadataFiles({} as never, {} as never);

/** Encodes text as UTF-8. */
const utf8 = (text: string): Uint8Array => new TextEncoder().encode(text);

/**
 * A render request's record with nothing in it.
 *
 * @returns The record.
 */
function empty(): MetadataFileRecord {
  return {
    present: [],
    genres: [],
    tags: [],
    abridgement: "unknown",
    expressionKind: "unspecified",
    explicitness: "unknown",
    identifiers: [],
    credits: [],
    creditRoles: [],
    seriesEntries: [],
    chapters: [],
  };
}

/**
 * Asserts a rejection with `invalid_params`.
 *
 * @param call - The pending call.
 * @param message - Pattern the message must match.
 */
async function invalidParams(call: Promise<unknown>, message: RegExp): Promise<void> {
  await assert.rejects(call, (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, "invalid_params");
    assert.match(err.message, message);
    return true;
  });
}

test("describe names the plugin", async () => {
  assert.deepEqual(await new Plugin({} as never, {} as never).describe(), {
    displayName: "Metadata files",
  });
});

test("health is always ready", async () => {
  assert.deepEqual(await files.health(), { ok: true, detail: "" });
});

test("parse routes to the dialect", async () => {
  const record = await files.parse({
    dialect: "libation_json",
    fileName: "Book.metadata.json",
    bytes: utf8('{"title":"Hi"}'),
  });
  assert.equal(record.title, "Hi");
});

test("a refused file or request becomes invalid_params", async () => {
  await invalidParams(
    files.parse({ dialect: "libation_json", fileName: "a.metadata.json", bytes: utf8("[") }),
    /not valid JSON/,
  );
  await invalidParams(
    files.render({
      dialect: "libation_json",
      record: empty(),
      fields: [],
      existing: utf8(""),
      audioFiles: [],
    }),
    /read-only/,
  );
});

test("a render hands the book's audio files to the dialect", async () => {
  const out = await files.render({
    dialect: "cue",
    record: { ...empty(), title: "Hi" },
    fields: ["title"],
    existing: utf8(""),
    audioFiles: [{ name: "book.mp3", durationMs: 0 }],
  });
  assert.match(new TextDecoder().decode(out), /^TITLE "Hi"\nFILE "book.mp3" MP3\n/);
});

test("other errors pass through unchanged", async () => {
  const sneaky = {
    get dialect(): never {
      throw new RangeError("unexpected");
    },
  };
  await assert.rejects(files.parse(sneaky as never), RangeError);
});
