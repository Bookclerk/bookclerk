/**
 * Unit tests for the entrypoint glue, with the SDK's isolate module replaced
 * by stand-ins (the real one imports `cloudflare:workers`, which only exists
 * inside workerd).
 */

import assert from "node:assert/strict";
import { mock, test } from "node:test";

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
  namedExports: { BookclerkEntrypoint: Entrypoint, MetadataEntrypoint: Entrypoint, PluginError },
});
const { Metadata, default: Plugin, guarded } = await import("../src/index.ts");

/** URLs the stubbed global `fetch` saw. */
const seen: string[] = [];
/** What the stubbed global `fetch` answers next. */
let reply: () => Response = () => new Response("", { status: 404 });
globalThis.fetch = (input: string | URL | Request) => {
  seen.push(String(input));
  return Promise.resolve().then(reply);
};

const metadata = new Metadata({} as never, { CONFIG: {} });
const ASIN = { scheme: "asin" as const, value: "B002V0QK4C" };

test("describe names the plugin", async () => {
  assert.deepEqual(await new Plugin({} as never, { CONFIG: {} }).describe(), {
    displayName: "Audnexus",
  });
});

test("health answers without a request", async () => {
  seen.length = 0;
  assert.deepEqual(await metadata.health(), { ok: true, detail: "" });
  assert.equal(seen.length, 0);
});

test("lookup and chapters go through global fetch", async () => {
  seen.length = 0;
  assert.deepEqual(await metadata.lookup({ identifiers: [ASIN] }), []);
  assert.equal((await metadata.chapters({ identifiers: [ASIN], marketplace: "uk" })).found, false);
  assert.deepEqual(seen, [
    "https://api.audnex.us/books/B002V0QK4C?region=us",
    "https://api.audnex.us/books/B002V0QK4C/chapters?region=uk",
  ]);
});

test("provider failures become PluginErrors with the same code", async () => {
  await assert.rejects(metadata.lookup({ identifiers: [] }), (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, "invalid_params");
    return true;
  });
  reply = () => new Response("", { status: 503 });
  await assert.rejects(metadata.chapters({ identifiers: [ASIN] }), (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, "unavailable");
    return true;
  });
});

test("other errors pass through unchanged", async () => {
  await assert.rejects(
    guarded(() => Promise.reject(new RangeError("unexpected"))),
    RangeError,
  );
});
