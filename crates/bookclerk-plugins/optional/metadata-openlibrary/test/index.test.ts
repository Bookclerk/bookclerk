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
const { Metadata, default: Plugin } = await import("../src/index.ts");

/** Requests the stubbed global `fetch` saw. */
const seen: { url: string; agent: string | null }[] = [];
/** What the stubbed global `fetch` answers next. */
let reply: () => Response = () => new Response(JSON.stringify({ docs: [] }));
globalThis.fetch = (input: string | URL | Request, init?: RequestInit) => {
  seen.push({ url: String(input), agent: new Headers(init?.headers).get("user-agent") });
  return Promise.resolve().then(reply);
};

const metadata = new Metadata({} as never, { CONFIG: { contact_email: "ops@example.org" } });

test("describe names the plugin", async () => {
  assert.deepEqual(
    await new Plugin({} as never, { CONFIG: { contact_email: undefined } }).describe(),
    { displayName: "Open Library" },
  );
});

test("health answers without a request", async () => {
  seen.length = 0;
  assert.deepEqual(await metadata.health(), { ok: true, detail: "" });
  assert.equal(seen.length, 0);
});

test("lookup and search go through global fetch with the contact email", async () => {
  seen.length = 0;
  assert.deepEqual(
    await metadata.lookup({ identifiers: [{ scheme: "isbn", value: "9780547928227" }] }),
    [],
  );
  assert.deepEqual(await metadata.search({ title: "Dune" }), []);
  assert.equal(seen.length, 2);
  assert.match(seen[0]?.agent ?? "", /\(ops@example\.org; /);
});

test("provider failures become PluginErrors with the same code", async () => {
  await assert.rejects(metadata.search({ title: " " }), (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, "invalid_params");
    return true;
  });
  reply = () => new Response("", { status: 503 });
  await assert.rejects(metadata.search({ title: "Dune" }), (err: unknown) => {
    assert.ok(err instanceof PluginError);
    assert.equal(err.code, "unavailable");
    return true;
  });
});

test("other errors pass through unchanged", async () => {
  // A thrown fetch is a ProviderFailure, so break the body read instead.
  reply = () =>
    ({
      status: 200,
      ok: true,
      headers: new Headers(),
      arrayBuffer: () => Promise.reject(new RangeError("unexpected")),
    }) as unknown as Response;
  await assert.rejects(metadata.search({ title: "Dune" }), RangeError);
});
