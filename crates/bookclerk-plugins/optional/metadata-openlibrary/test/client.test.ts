/**
 * Unit tests for the Open Library client against a scripted `fetch`.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { MAX_BODY_BYTES, lookup, search, type ClientOptions, type Fetch } from "../src/client.ts";
import { ProviderFailure } from "../src/openlibrary.ts";

/** One request the scripted `fetch` saw. */
interface Seen {
  /** Requested URL. */
  url: string;
  /** Request headers. */
  headers: Headers;
}

/**
 * Client options whose `fetch` answers with `reply` and records each call.
 *
 * @param reply - Builds the response, or throws to simulate a network error.
 * @returns The options and the list of requests seen.
 */
function scripted(reply: () => Response): { options: ClientOptions; seen: Seen[] } {
  const seen: Seen[] = [];
  const fetch: Fetch = (url, init) => {
    seen.push({ url, headers: new Headers(init.headers) });
    return Promise.resolve().then(reply);
  };
  return { options: { fetch, version: "0.1.0", contactEmail: "ops@example.org" }, seen };
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

const HOBBIT = readFileSync(new URL("fixtures/search-isbn.json", import.meta.url), "utf8");

test("lookup sends one identified request and maps the answer", async () => {
  const { options, seen } = scripted(() => new Response(HOBBIT, { status: 200 }));
  const records = await lookup(options, {
    identifiers: [{ scheme: "isbn", value: "978-0-547-92822-7" }],
  });
  assert.equal(records.length, 1);
  assert.equal(records[0]?.publisher, "Mariner Books");
  assert.equal(seen.length, 1);
  assert.match(
    seen[0]?.url ?? "",
    /^https:\/\/openlibrary\.org\/search\.json\?isbn=9780547928227&/,
  );
  assert.equal(
    seen[0]?.headers.get("user-agent"),
    "Bookclerk-OpenLibrary/0.1.0 (ops@example.org; https://bookclerk.app)",
  );
  assert.equal(seen[0]?.headers.get("accept"), "application/json");
});

test("lookup with no served identifier makes no request", async () => {
  const { options, seen } = scripted(() => new Response(HOBBIT));
  assert.deepEqual(
    await lookup(options, { identifiers: [{ scheme: "asin", value: "B0036S4B2G" }] }),
    [],
  );
  assert.equal(seen.length, 0);
});

test("search filters hits by title", async () => {
  const { options, seen } = scripted(() => new Response(HOBBIT));
  assert.equal((await search(options, { title: "Hobbit" })).length, 1);
  assert.equal((await search(options, { title: "Dune" })).length, 0);
  assert.equal(seen.length, 2);
});

test("a 404 is no match", async () => {
  const { options } = scripted(() => new Response("", { status: 404 }));
  assert.deepEqual(await search(options, { title: "Hobbit" }), []);
});

test("error statuses are unavailable", async () => {
  for (const status of [429, 500, 503, 403]) {
    const { options } = scripted(() => new Response("", { status }));
    await rejectsWith(
      search(options, { title: "Hobbit" }),
      "unavailable",
      new RegExp(`HTTP ${status}`),
    );
  }
});

test("a network error is unavailable", async () => {
  const { options } = scripted(() => {
    throw new TypeError("connection reset");
  });
  await rejectsWith(
    search(options, { title: "Hobbit" }),
    "unavailable",
    /request failed: connection reset/,
  );
  const { options: odd } = scripted(() => {
    throw "boom";
  });
  await rejectsWith(search(odd, { title: "Hobbit" }), "unavailable", /request failed: boom/);
});

test("a body that is not JSON is unavailable", async () => {
  const { options } = scripted(() => new Response("<html>"));
  await rejectsWith(search(options, { title: "Hobbit" }), "unavailable", /not JSON/);
});

test("an oversized body is refused before and after reading", async () => {
  const declared = scripted(
    () => new Response("{}", { headers: { "content-length": String(MAX_BODY_BYTES + 1) } }),
  );
  await rejectsWith(search(declared.options, { title: "Hobbit" }), "unavailable", /too large/);
  const actual = scripted(() => new Response(" ".repeat(MAX_BODY_BYTES + 1)));
  await rejectsWith(search(actual.options, { title: "Hobbit" }), "unavailable", /too large/);
});
