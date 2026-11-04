/**
 * Unit tests for the Audnexus client against a scripted `fetch`.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { ProviderFailure } from "../src/audnexus.ts";
import {
  MAX_BODY_BYTES,
  chapters,
  getJson,
  lookup,
  readCapped,
  type ClientOptions,
  type Fetch,
} from "../src/client.ts";

/**
 * Client options whose `fetch` answers each URL from `routes` (404 when a
 * URL has no route) and records the URLs it saw.
 *
 * @param routes - Response builders by URL.
 * @returns The options and the URLs requested, in order.
 */
function scripted(routes: Record<string, () => Response>): {
  options: ClientOptions;
  seen: string[];
  agents: string[];
} {
  const seen: string[] = [];
  const agents: string[] = [];
  const fetch: Fetch = (url, init) => {
    seen.push(url);
    agents.push(new Headers(init.headers).get("user-agent") ?? "");
    const route = routes[url];
    return Promise.resolve().then(() => (route ? route() : new Response("", { status: 404 })));
  };
  return { options: { fetch, version: "0.1.0" }, seen, agents };
}

/**
 * Asserts that `promise` rejects with a `ProviderFailure` of `code`.
 *
 * @param promise - The call under test.
 * @param code - Expected failure code.
 * @param message - Pattern the message must match.
 */
async function rejectsWith(
  promise: Promise<unknown>,
  code: string,
  message: RegExp,
): Promise<void> {
  await assert.rejects(promise, (err: unknown) => {
    assert.ok(err instanceof ProviderFailure);
    assert.equal(err.code, code);
    assert.match(err.message, message);
    return true;
  });
}

const BOOK = readFileSync(new URL("fixtures/book.json", import.meta.url), "utf8");
const CHAPTERS = readFileSync(new URL("fixtures/chapters.json", import.meta.url), "utf8");
/**
 * A chunked body that never ends, counting how many chunks were pulled.
 *
 * @returns The stream and a reader of its state.
 */
function endless(): {
  body: ReadableStream<Uint8Array>;
  state: { pulled: number; cancelled: boolean };
} {
  const state = { pulled: 0, cancelled: false };
  const chunk = new Uint8Array(64 * 1024).fill(0x20);
  const body = new ReadableStream<Uint8Array>({
    pull(controller) {
      state.pulled++;
      controller.enqueue(chunk);
    },
    cancel() {
      state.cancelled = true;
    },
  });
  return { body, state };
}

/**
 * A body that sends one chunk and then fails, as a dropped connection does.
 *
 * @returns The stream.
 */
function broken(): ReadableStream<Uint8Array> {
  let sent = false;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (sent) {
        controller.error(new TypeError("connection reset"));
        return;
      }
      sent = true;
      controller.enqueue(new TextEncoder().encode("{"));
    },
  });
}

const BOOK_URL = "https://api.audnex.us/books/B002V0QK4C?region=us";
const CHAPTERS_URL = "https://api.audnex.us/books/B002V0QK4C/chapters?region=us";

test("lookup asks once per ASIN and skips unknown ones", async () => {
  const { options, seen, agents } = scripted({ [BOOK_URL]: () => new Response(BOOK) });
  const records = await lookup(options, {
    identifiers: [
      { scheme: "asin", value: "B002V0QK4C" },
      { scheme: "asin", value: "B000000001" },
    ],
    marketplace: "us",
    limit: 5,
  });
  assert.deepEqual(
    records.map((r) => r.title),
    ["Wizard's First Rule"],
  );
  assert.deepEqual(seen, [BOOK_URL, "https://api.audnex.us/books/B000000001?region=us"]);
  assert.equal(agents[0], "Bookclerk-Audnexus/0.1.0 (https://bookclerk.app)");
});

test("lookup drops a body Audnexus sends for an unknown ASIN", async () => {
  const { options } = scripted({
    [BOOK_URL]: () => new Response(JSON.stringify({ error: "not found" })),
  });
  assert.deepEqual(
    await lookup(options, { identifiers: [{ scheme: "asin", value: "B002V0QK4C" }] }),
    [],
  );
});

test("chapters reads the first ASIN's layout", async () => {
  const { options, seen } = scripted({ [CHAPTERS_URL]: () => new Response(CHAPTERS) });
  const layout = await chapters(options, {
    identifiers: [
      { scheme: "isbn", value: "9781423377962" },
      { scheme: "asin", value: "B002V0QK4C" },
      { scheme: "asin", value: "B000000001" },
    ],
  });
  assert.equal(layout.found, true);
  assert.equal(layout.chapters.length, 4);
  assert.deepEqual(seen, [CHAPTERS_URL]);
});

test("chapters is not found without an ASIN or on 404", async () => {
  const { options, seen } = scripted({});
  assert.equal(
    (await chapters(options, { identifiers: [{ scheme: "isbn", value: "9781423377962" }] })).found,
    false,
  );
  assert.equal(seen.length, 0);
  assert.equal(
    (await chapters(options, { identifiers: [{ scheme: "asin", value: "B002V0QK4C" }] })).found,
    false,
  );
  assert.equal(seen.length, 1);
});

test("error statuses and network errors are unavailable", async () => {
  for (const status of [429, 500, 503]) {
    const { options } = scripted({ [BOOK_URL]: () => new Response("", { status }) });
    await rejectsWith(getJson(options, BOOK_URL), "unavailable", new RegExp(`HTTP ${status}`));
  }
  const down = scripted({
    [BOOK_URL]: () => {
      throw new TypeError("connection reset");
    },
  });
  await rejectsWith(
    getJson(down.options, BOOK_URL),
    "unavailable",
    /request failed: connection reset/,
  );
  const odd = scripted({
    [BOOK_URL]: () => {
      throw "boom";
    },
  });
  await rejectsWith(getJson(odd.options, BOOK_URL), "unavailable", /request failed: boom/);
});

test("bodies that are not JSON or too large are unavailable", async () => {
  const html = scripted({ [BOOK_URL]: () => new Response("<html>") });
  await rejectsWith(getJson(html.options, BOOK_URL), "unavailable", /not JSON/);
  const declared = scripted({
    [BOOK_URL]: () =>
      new Response("{}", { headers: { "content-length": String(MAX_BODY_BYTES + 1) } }),
  });
  await rejectsWith(getJson(declared.options, BOOK_URL), "unavailable", /too large/);
  const actual = scripted({ [BOOK_URL]: () => new Response(" ".repeat(MAX_BODY_BYTES + 1)) });
  await rejectsWith(getJson(actual.options, BOOK_URL), "unavailable", /too large/);
});

test("a body without a length is cut off at the cap", async () => {
  const { body, state } = endless();
  const chunked = scripted({ [BOOK_URL]: () => new Response(body) });
  await rejectsWith(getJson(chunked.options, BOOK_URL), "unavailable", /too large/);
  assert.equal(state.cancelled, true);
  assert.ok(state.pulled * 64 * 1024 <= MAX_BODY_BYTES + 2 * 64 * 1024);
});

test("readCapped reads a missing body as empty", async () => {
  assert.deepEqual(await readCapped(new Response(null), 10), new Uint8Array());
});

test("a body that fails after the headers is unavailable", async () => {
  const { options } = scripted({ [BOOK_URL]: () => new Response(broken()) });
  await rejectsWith(getJson(options, BOOK_URL), "unavailable", /response failed: connection reset/);
  const { options: odd } = scripted({
    [BOOK_URL]: () =>
      ({
        status: 200,
        ok: true,
        headers: new Headers(),
        body: { getReader: () => ({ read: () => Promise.reject("boom") }) },
      }) as unknown as Response,
  });
  await rejectsWith(getJson(odd, BOOK_URL), "unavailable", /response failed: boom/);
});
