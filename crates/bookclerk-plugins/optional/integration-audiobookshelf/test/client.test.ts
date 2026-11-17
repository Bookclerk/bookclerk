/**
 * The Audiobookshelf REST client against a fake server.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  AbsClient,
  AbsFailure,
  BudgetSpent,
  MAX_BODY_BYTES,
  RequestBudget,
  checkBaseUrl,
  codeForStatus,
  isLoopbackHostport,
  itemMetadataFrom,
  userDetailFrom,
} from "../src/client.ts";
import { fakeServer, json } from "./support.ts";

/**
 * A client for `server` with key `k`.
 *
 * @param server - The fake server.
 * @param budget - Request budget.
 * @returns The client.
 */
function clientFor(server: ReturnType<typeof fakeServer>, budget?: RequestBudget): AbsClient {
  return new AbsClient(`${server.baseUrl}/`, "test-key", server.fetch, budget);
}

test("the base URL must be https unless the host is loopback", () => {
  assert.equal(checkBaseUrl("HTTPS://abs.example/"), "HTTPS://abs.example");
  assert.equal(checkBaseUrl(" http://127.0.0.1:13378 "), "http://127.0.0.1:13378");
  assert.equal(checkBaseUrl("HTTP://localhost:13378"), "HTTP://localhost:13378");
  assert.equal(checkBaseUrl("http://[::1]:80/abs"), "http://[::1]:80/abs");
  assert.equal(checkBaseUrl("http://user@localhost"), "http://user@localhost");
  for (const bad of [
    "",
    "http://abs.example",
    "ftp://abs.example",
    "abs.example",
    "http://localhost.evil.example",
    "https://",
    // `fetch` reads `\\` as a path separator, so this host is abs.example.
    "http://abs.example\\@127.0.0.1",
  ]) {
    assert.throws(
      () => checkBaseUrl(bad),
      (err: unknown) => err instanceof AbsFailure && err.code === "invalid_params",
      bad,
    );
  }
  assert.equal(isLoopbackHostport("localhost:"), true);
  assert.equal(checkBaseUrl("HTTP://[::1]"), "HTTP://[::1]");
  assert.equal(isLoopbackHostport("127.0.0.2"), false);
});

test("authorize sends the key as a Bearer token and never follows redirects", async () => {
  const server = fakeServer({
    "POST /api/authorize": () => json({ user: { id: "u1", username: "admin" } }),
  });
  const user = await clientFor(server).authorize();
  assert.deepEqual(user, { id: "u1", username: "admin", token: null });
  assert.equal(server.seen[0]?.headers["authorization"], "Bearer test-key");
  assert.equal(server.seen[0]?.redirect, "manual");
});

test("an error status becomes a typed failure quoting the body", async () => {
  const server = fakeServer({
    "POST /api/authorize": () => new Response("Unauthorized", { status: 401 }),
    "GET /api/libraries": () => new Response("", { status: 500 }),
    "GET /api/users": () => new Response("<html>", { status: 200 }),
  });
  const client = clientFor(server);
  await assert.rejects(client.authorize(), (err: unknown) => {
    assert.ok(err instanceof AbsFailure);
    assert.equal(err.code, "unauthorized");
    assert.equal(err.status, 401);
    assert.match(err.message, /HTTP 401: Unauthorized/);
    return true;
  });
  await assert.rejects(client.listLibraries(), /HTTP 500$/);
  await assert.rejects(client.listUsers(), /not JSON/);
  // A redirect is a failure, never followed.
  const moved = fakeServer({
    "POST /login": () =>
      new Response(null, { status: 307, headers: { location: "http://evil.example/" } }),
  });
  await assert.rejects(clientFor(moved).authenticateUser("bob", "pw"), /HTTP 307/);
  assert.equal(moved.seen.length, 1);
});

test("status codes map to plugin error codes", () => {
  assert.equal(codeForStatus(401), "unauthorized");
  assert.equal(codeForStatus(403), "forbidden");
  assert.equal(codeForStatus(404), "not_found");
  assert.equal(codeForStatus(429), "rate_limited");
  assert.equal(codeForStatus(400), "invalid_params");
  assert.equal(codeForStatus(503), "unavailable");
  assert.equal(codeForStatus(302), "unavailable");
});

test("a request that fails or a body that is too large is unavailable", async () => {
  const failing = new AbsClient("https://abs.example", "k", () =>
    Promise.reject(new Error("refused")),
  );
  await assert.rejects(failing.listUsers(), (err: unknown) => {
    assert.ok(err instanceof AbsFailure);
    assert.equal(err.code, "unavailable");
    assert.match(err.message, /refused/);
    return true;
  });
  const huge = fakeServer({
    "GET /api/users": () => new Response(new Uint8Array(MAX_BODY_BYTES + 1)),
  });
  await assert.rejects(clientFor(huge).listUsers(), /too large/);
});

test("scan posts to the library and asks for force only when told", async () => {
  const server = fakeServer({
    "POST /api/libraries/lib%201/scan": () => new Response(null, { status: 200 }),
    "POST /api/libraries/lib%201/scan?force=1": () => new Response(null, { status: 204 }),
    "POST /api/libraries/other/scan": () => json({}, 201),
  });
  const client = clientFor(server);
  await client.scanLibrary("lib 1", false);
  await client.scanLibrary("lib 1", true);
  await assert.rejects(client.scanLibrary("other", false), /HTTP 201/);
  assert.deepEqual(
    server.seen.map((s) => s.path),
    [
      "/api/libraries/lib%201/scan",
      "/api/libraries/lib%201/scan?force=1",
      "/api/libraries/other/scan",
    ],
  );
});

test("login maps the user and sends the password only in the body", async () => {
  const server = fakeServer({
    "POST /login": () => json({ user: { id: "usr_1", username: "bob", token: "tok" } }),
  });
  const password = ["sec", "ret"].join("");
  const user = await clientFor(server).authenticateUser("bob", password);
  assert.deepEqual(user, {
    provider: "audiobookshelf",
    externalUserId: "usr_1",
    displayName: "bob",
    accessToken: "tok",
  });
  const seen = server.seen[0];
  assert.equal(seen?.headers["authorization"], undefined, "login sends no API key");
  assert.deepEqual(JSON.parse(seen?.body ?? ""), { username: "bob", password });
  assert.ok(!seen?.path.includes(password));

  const empty = fakeServer({ "POST /login": () => json({}) });
  await assert.rejects(clientFor(empty).authenticateUser("bob", password), (err: unknown) => {
    assert.ok(err instanceof AbsFailure);
    assert.equal(err.code, "unauthorized");
    return true;
  });
});

test("users, libraries and items are read leniently", async () => {
  const server = fakeServer({
    "GET /api/users": () =>
      json({ users: [{ id: "a", username: "ada" }, { username: "no id" }, { id: "b" }] }),
    "GET /api/libraries": () => json({ libraries: [{ id: "l1", name: "Books" }, { id: "l2" }] }),
    "GET /api/items/i%2F1": () =>
      json({
        media: {
          metadata: {
            title: "T",
            authors: [{ name: "A" }, { name: " " }, { name: "B" }],
            asin: "b0x",
          },
        },
      }),
  });
  const client = clientFor(server);
  assert.deepEqual(
    (await client.listUsers()).map((u) => [u.id, u.username]),
    [
      ["a", "ada"],
      ["b", ""],
    ],
  );
  assert.deepEqual(await client.listLibraries(), [{ id: "l1", name: "Books" }]);
  assert.deepEqual(await client.getItemMetadata("i/1"), {
    title: "T",
    authorName: "A, B",
    asin: "b0x",
    isbn: null,
  });
  assert.equal(
    itemMetadataFrom({ media: { metadata: { authorName: "Z", authors: [{ name: "A" }] } } })
      .authorName,
    "Z",
  );
  assert.deepEqual(itemMetadataFrom(null), {
    title: null,
    authorName: null,
    asin: null,
    isbn: null,
  });
});

test("a user's progress rows keep only well-formed fields", () => {
  const detail = userDetailFrom({
    id: "u1",
    mediaProgress: [
      {
        libraryItemId: "li1",
        progress: 0.5,
        currentTime: 12,
        duration: "x",
        isFinished: true,
        lastUpdate: 7,
      },
      { progress: 1 },
      { libraryItemId: "li2", progress: Number.NaN },
    ],
  });
  assert.deepEqual(detail.mediaProgress, [
    {
      libraryItemId: "li1",
      duration: null,
      progress: 0.5,
      currentTime: 12,
      isFinished: true,
      lastUpdate: 7,
    },
    {
      libraryItemId: "li2",
      duration: null,
      progress: null,
      currentTime: null,
      isFinished: false,
      lastUpdate: null,
    },
  ]);
  assert.throws(() => userDetailFrom({ mediaProgress: [] }), AbsFailure);
});

test("the budget stops requests once spent", async () => {
  const server = fakeServer({ "GET /api/users": () => json({ users: [] }) });
  const client = clientFor(server, new RequestBudget(1));
  await client.listUsers();
  assert.equal(client.budget.left, 0);
  await assert.rejects(client.listUsers(), BudgetSpent);
  assert.equal(server.seen.length, 1);
});
