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
    OidcEntrypoint: Entrypoint,
    RemoteLibraryEntrypoint: Entrypoint,
    PluginError,
  },
});
const { AbsFailure } = await import("../src/client.ts");
const {
  MetadataFiles,
  Oidc,
  OIDC_CLIENT,
  RemoteLibrary,
  default: Plugin,
  contextOf,
  guarded,
} = await import("../src/index.ts");

/** Requests the stubbed global `fetch` saw, as `METHOD url`. */
const seen: string[] = [];
/** What the stubbed global `fetch` answers next. */
let reply: () => Response = () => new Response("", { status: 404 });
globalThis.fetch = (input: string | URL | Request, init?: RequestInit) => {
  seen.push(`${init?.method ?? "GET"} ${String(input)}`);
  return Promise.resolve().then(reply);
};

/** Bindings with an API key and a library. */
const ENV = {
  CONFIG: { base_url: "https://abs.example", library_id: "lib1" },
  SECRETS: { api_key: "k" },
};

test("describe names the plugin and its alias", async () => {
  const described = await new Plugin({} as never, ENV as never).describe();
  assert.equal(described.displayName, "Audiobookshelf");
  assert.deepEqual(described.aliases, ["abs"]);
  assert.equal(described.brand?.id, "audiobookshelf");
});

test("guarded turns a typed failure into a plugin error with its code", async () => {
  await assert.rejects(
    guarded(() => Promise.reject(new AbsFailure("forbidden", "no"))),
    (err: unknown) =>
      err instanceof PluginError && err.code === "forbidden" && err.message === "no",
  );
  await assert.rejects(
    guarded(() => Promise.reject(new TypeError("other"))),
    TypeError,
  );
  assert.equal(await guarded(() => Promise.resolve(3)), 3);
});

test("the context reads settings and bindings from env", () => {
  const ctx = contextOf({ ...ENV, LISTENING_STATE: undefined, SOCKETS: undefined } as never);
  assert.equal(ctx.settings.apiKey, "k");
  assert.equal(ctx.settings.libraryId, "lib1");
  assert.equal(ctx.db, undefined);
});

test("book_acquired is acknowledged after the rescan, or retried when it fails", async () => {
  const plugin = new Plugin({} as never, ENV as never);
  const outcomes: string[] = [];
  /**
   * A delivered event that records what was done with it.
   *
   * @param type - Event type.
   * @returns The message.
   */
  const message = (type: string): unknown => ({
    type,
    ack: () => outcomes.push(`${type}:ack`),
    retry: (options: { reason: string }) => outcomes.push(`${type}:retry:${options.reason}`),
  });
  seen.length = 0;
  reply = () => new Response(null, { status: 200 });
  await plugin.event({ messages: [message("book_acquired"), message("other")] } as never);
  reply = () => new Response("busy", { status: 503 });
  await plugin.event({ messages: [message("book_acquired")] } as never);
  assert.deepEqual(seen, [
    "POST https://abs.example/api/libraries/lib1/scan",
    "POST https://abs.example/api/libraries/lib1/scan",
  ]);
  assert.deepEqual(outcomes.slice(0, 2), ["book_acquired:ack", "other:ack"]);
  assert.match(outcomes[2] ?? "", /^book_acquired:retry:.*HTTP 503/);
});

test("a pushed user_added frame asks the host to poll", async () => {
  const plugin = new Plugin({} as never, ENV as never);
  const ws = { send: () => Promise.resolve(), close: () => Promise.resolve() };
  assert.deepEqual(await plugin.webSocketMessage(ws as never, '42["user_added",{}]'), {
    pollEvents: true,
  });
  assert.deepEqual(await plugin.webSocketMessage(ws as never, "3"), { pollEvents: false });
});

test("migrations are offered for LISTENING_STATE", () => {
  const plugin = new Plugin({} as never, ENV as never);
  assert.equal(plugin.databaseMigrations("LISTENING_STATE").length, 2);
  assert.deepEqual(plugin.databaseMigrations("OTHER"), []);
});

test("remoteLibrary methods reach the server and map failures", async () => {
  const library = new RemoteLibrary({} as never, ENV as never);
  seen.length = 0;
  reply = () => new Response(null, { status: 204 });
  await library.scanLibrary({ force: true } as never);
  assert.deepEqual(seen, ["POST https://abs.example/api/libraries/lib1/scan?force=1"]);
  reply = () => new Response("no", { status: 401 });
  assert.equal((await library.health()).ok, false);
  await assert.rejects(
    library.diagnose(),
    (err: unknown) => err instanceof PluginError && err.code === "unauthorized",
  );
  await assert.rejects(
    library.pollEvents(),
    (err: unknown) => err instanceof PluginError && err.code === "unauthorized",
  );
  await assert.rejects(library.syncListening(), (err: unknown) => err instanceof PluginError);
  const unconfigured = new RemoteLibrary({} as never, { CONFIG: {}, SECRETS: {} } as never);
  await assert.rejects(
    unconfigured.start(),
    (err: unknown) => err instanceof PluginError && err.code === "invalid_params",
  );
});

test("oidc offers the Audiobookshelf client and signs people in", async () => {
  const oidc = new Oidc({} as never, ENV as never);
  assert.deepEqual(await oidc.clients(), [OIDC_CLIENT]);
  reply = () =>
    new Response(JSON.stringify({ user: { id: "u1", username: "bob", token: "t" } }), {
      headers: { "content-type": "application/json" },
    });
  assert.deepEqual(await oidc.authenticateUser({ username: "bob", password: "pw" }), {
    provider: "audiobookshelf",
    externalUserId: "u1",
    displayName: "bob",
    accessToken: "t",
  });
});

/** The `metadataFiles` entrypoint; it reads no bindings. */
const files = new MetadataFiles({} as never, {} as never);

/**
 * Encodes text as UTF-8.
 *
 * @param text - Text.
 * @returns Its bytes.
 */
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

test("metadataFiles is always ready", async () => {
  assert.deepEqual(await files.health(), { ok: true, detail: "" });
});

test("metadataFiles parse and render route to the dialect", async () => {
  const record = await files.parse({
    dialect: "desc_txt",
    fileName: "desc.txt",
    bytes: utf8("Hi"),
  });
  assert.equal(record.description, "Hi");
  const bytes = await files.render({
    dialect: "desc_txt",
    record: { ...empty(), present: ["description"], description: "New" },
    fields: ["description"],
    existing: utf8("Old"),
    audioFiles: [],
  });
  assert.equal(new TextDecoder().decode(bytes), "New\n");
});

test("a refused file or request becomes invalid_params", async () => {
  await invalidParams(
    files.parse({ dialect: "abs_metadata_json", fileName: "metadata.json", bytes: utf8("[") }),
    /not valid JSON/,
  );
  await invalidParams(
    files.render({
      dialect: "opf",
      record: empty(),
      fields: [],
      existing: utf8(""),
      audioFiles: [],
    }),
    /unknown dialect/,
  );
});

test("other errors pass through a dialect call unchanged", async () => {
  const sneaky = {
    get dialect(): never {
      throw new RangeError("unexpected");
    },
  };
  await assert.rejects(files.parse(sneaky as never), RangeError);
});
