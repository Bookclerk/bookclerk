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
  namedExports: { BookclerkEntrypoint: Entrypoint, CoverEntrypoint: Entrypoint, PluginError },
});
const { Covers, default: Plugin } = await import("../src/index.ts");

/** URLs the stubbed global `fetch` saw. */
const seen: string[] = [];
globalThis.fetch = (input: string | URL | Request) => {
  seen.push(String(input));
  return Promise.resolve(
    new Response(new Uint8Array([0xff, 0xd8]), { headers: { "content-type": "image/jpeg" } }),
  );
};

const covers = new Covers({} as never, {} as never);

/** A sink that drains each body. */
const sink = {
  /**
   * Drains `body`.
   *
   * @param key - Sink key.
   * @param body - Body stream.
   * @returns The key.
   */
  async put(key: string, body: ReadableStream<Uint8Array>): Promise<{ key: string }> {
    await new Response(body).arrayBuffer();
    return { key };
  },
};

test("describe names the plugin", async () => {
  assert.deepEqual(await new Plugin({} as never, {} as never).describe(), {
    displayName: "Cover images",
  });
});

test("health answers without a request", async () => {
  seen.length = 0;
  assert.deepEqual(await covers.health(), { ok: true, detail: "" });
  assert.equal(seen.length, 0);
});

test("fetchCovers goes through global fetch", async () => {
  seen.length = 0;
  const images = await covers.fetchCovers(
    { urls: ["https://a.example/1.jpg"], maxBytes: 16 },
    sink as never,
  );
  assert.deepEqual(images, [
    { url: "https://a.example/1.jpg", stored: true, contentType: "image/jpeg" },
  ]);
  assert.deepEqual(seen, ["https://a.example/1.jpg"]);
});

test("a malformed request becomes invalid_params", async () => {
  await assert.rejects(
    covers.fetchCovers({ urls: [], maxBytes: 1 }, sink as never),
    (err: unknown) => {
      assert.ok(err instanceof PluginError);
      assert.equal(err.code, "invalid_params");
      return true;
    },
  );
});

test("other errors pass through unchanged", async () => {
  const sneaky = {
    get urls(): never {
      throw new RangeError("unexpected");
    },
  };
  await assert.rejects(covers.fetchCovers(sneaky as never, sink as never), RangeError);
});
