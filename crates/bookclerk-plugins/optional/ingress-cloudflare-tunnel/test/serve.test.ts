/**
 * Serving through the library: large responses that can and cannot be
 * fetched in ranges, continuations the library answers badly, and
 * malformed upgrade keys.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  MAX_BUFFERED_RESPONSE,
  continueBody,
  serveRequest,
  serveUpgrade,
  type Continuation,
} from "../src/serve.ts";
import { FakeLibrary } from "./fake.ts";

/** A body larger than the buffer. */
const LARGE = new Uint8Array(MAX_BUFFERED_RESPONSE + 10);

/** A GET for `/`. */
const GET = { method: "GET", target: "/", headers: [] };

/**
 * Serves a GET against a library that gives one response.
 *
 * @param response - Builds the response.
 * @returns The answer's status.
 */
async function statusOf(response: () => Response): Promise<number> {
  const answer = await serveRequest(
    new FakeLibrary(response),
    "books.example.com",
    GET,
    null,
    undefined,
  );
  return answer.status;
}

test("a large response that cannot be ranged is a 502", async () => {
  // 200 without byte ranges, and a 200 with an empty length.
  assert.equal(
    await statusOf(
      () => new Response(LARGE, { headers: { "content-length": String(LARGE.length) } }),
    ),
    502,
  );
  assert.equal(
    await statusOf(
      () => new Response(LARGE, { headers: { "content-length": "0", "accept-ranges": "bytes" } }),
    ),
    502,
  );
  // A 206 whose range disagrees with its length, a valid 206, and another status.
  assert.equal(
    await statusOf(
      () =>
        new Response(LARGE, {
          status: 206,
          headers: { "content-length": String(LARGE.length), "content-range": "bytes 0-9/100" },
        }),
    ),
    502,
  );
  assert.equal(
    await statusOf(
      () =>
        new Response(LARGE, {
          status: 206,
          headers: {
            "content-length": String(LARGE.length),
            "content-range": `bytes 5-${LARGE.length + 4}/${LARGE.length + 100}`,
          },
        }),
    ),
    206,
  );
  assert.equal(
    await statusOf(
      () =>
        new Response(LARGE, { status: 203, headers: { "content-length": String(LARGE.length) } }),
    ),
    502,
  );
});

test("a continuation the library fails or answers wrongly ends the body", async () => {
  const more: Continuation = {
    target: "/",
    headers: [],
    clientAddress: undefined,
    next: 10,
    end: 99,
    total: 100,
    validator: null,
  };
  const throwing = new FakeLibrary(() => {
    throw new Error("down");
  });
  assert.equal(await continueBody(throwing, "books.example.com", more), null);
  const wrong = new FakeLibrary(() => new Response("whole", { status: 200 }));
  assert.equal(await continueBody(wrong, "books.example.com", more), null);
});

test("an upgrade with a malformed key is a 400", async () => {
  const answer = await serveUpgrade(
    new FakeLibrary(() => new Response("ok")),
    "books.example.com",
    {
      method: "GET",
      target: "/",
      headers: [
        ["sec-websocket-version", "13"],
        ["sec-websocket-key", "short"],
      ],
    },
    undefined,
    3,
  );
  assert.equal(answer.kind === "answer" && answer.answer.status, 400);
});
