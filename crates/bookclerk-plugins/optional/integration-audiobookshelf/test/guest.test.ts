/**
 * The entrypoint methods apart from the SDK: settings, the user watch
 * across plugin starts, the pushed-event socket, events and sign-in.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import { TEST_DATABASE_PROFILES, createTestDatabase } from "@bookclerk/plugin-sdk/testkit";
import type { HostWebSocket, SocketsBinding } from "@bookclerk/plugin-sdk/workerd";

import { AbsFailure } from "../src/client.ts";
import * as guest from "../src/guest.ts";
import { migrationsFor } from "../src/listening.ts";
import { readSettings, type Settings } from "../src/settings.ts";
import { fakeServer, json, usersServer, type FakeServer } from "./support.ts";

/** Settings with an API key and every switch on. */
const SETTINGS: Settings = {
  baseUrl: "https://abs.example",
  apiKey: "secret-key",
  libraryId: "lib1",
  watchUsers: true,
  notifyScanOnAcquire: true,
  allowCredentialLogin: true,
};

/** A `SOCKETS` stand-in recording what it was asked. */
interface FakeSockets {
  /** The binding. */
  readonly binding: SocketsBinding;
  /** Every `connect` call. */
  readonly connects: Array<{ url: string; options: unknown }>;
  /** Sockets that count as open. */
  open: number;
}

/**
 * A `SOCKETS` stand-in.
 *
 * @returns The fake.
 */
function fakeSockets(): FakeSockets {
  const fake: FakeSockets = {
    connects: [],
    open: 0,
    binding: {
      connect: (url: string, options?: unknown) => {
        fake.connects.push({ url, options });
        fake.open += 1;
        return Promise.resolve({} as HostWebSocket);
      },
      getWebSockets: () =>
        Promise.resolve(Array.from({ length: fake.open }, () => ({}) as HostWebSocket)),
    } as unknown as SocketsBinding,
  };
  return fake;
}

/** A `HostWebSocket` stand-in recording what was sent and closed. */
interface FakeSocket {
  /** The socket. */
  readonly ws: HostWebSocket;
  /** Frames sent. */
  readonly sent: string[];
  /** Close codes. */
  readonly closed: number[];
}

/**
 * A `HostWebSocket` stand-in.
 *
 * @returns The fake.
 */
function fakeSocket(): FakeSocket {
  const sent: string[] = [];
  const closed: number[] = [];
  const ws = {
    send: (frame: string) => {
      sent.push(frame);
      return Promise.resolve();
    },
    close: (code: number) => {
      closed.push(code);
      return Promise.resolve();
    },
  } as unknown as HostWebSocket;
  return { ws, sent, closed };
}

/**
 * A context over `server`.
 *
 * @param server - The fake server.
 * @param extra - Settings and bindings to change.
 * @returns The context.
 */
function ctxFor(
  server: FakeServer,
  extra: Partial<Settings> & Pick<guest.Context, "db" | "sockets"> = {},
): guest.Context {
  const { db, sockets, ...settings } = extra;
  return { settings: { ...SETTINGS, ...settings }, fetch: server.fetch, db, sockets };
}

test("settings read the key from the instance or the secrets, and default the switches on", () => {
  const s = readSettings({ base_url: " https://abs.example ", library_id: " " }, { apiKey: "k" });
  assert.equal(s.apiKey, "k");
  assert.equal(s.libraryId, null);
  assert.equal(s.watchUsers, true);
  assert.equal(s.notifyScanOnAcquire, true);
  assert.equal(s.allowCredentialLogin, true);
  const off = readSettings(
    {
      api_key: "a",
      watch_users: false,
      notify_scan_on_acquire: false,
      allow_credential_login: false,
    },
    { api_key: "b" },
  );
  assert.equal(off.apiKey, "a");
  assert.equal(off.watchUsers, false);
  assert.equal(off.notifyScanOnAcquire, false);
  assert.equal(off.allowCredentialLogin, false);
  assert.equal(readSettings({}, { api_key: "  " }).apiKey, null);
  assert.equal(readSettings(null, null).apiKey, null);
});

test("health reports a missing key without a request", async () => {
  const server = usersServer([]);
  const report = await guest.health(ctxFor(server, { apiKey: null }));
  assert.equal(report.ok, false);
  assert.match(report.detail, /api_key/);
  assert.equal(server.seen.length, 0);
});

test("health and diagnose name the key's user and libraries", async () => {
  const server = fakeServer({
    "POST /api/authorize": () => json({ user: { id: "u1", username: "admin" } }),
    "GET /api/libraries": () => json({ libraries: [{ id: "l1", name: "Books" }] }),
  });
  assert.deepEqual(await guest.health(ctxFor(server)), { ok: true, detail: "authorized as admin" });
  assert.deepEqual(await guest.diagnose(ctxFor(server)), [
    "authorized as admin (u1)",
    "library l1 — Books",
  ]);
  const anonymous = fakeServer({
    "POST /api/authorize": () => json({}),
    "GET /api/libraries": () => json({ libraries: [] }),
  });
  assert.deepEqual(await guest.health(ctxFor(anonymous)), { ok: true, detail: "" });
  assert.deepEqual(await guest.diagnose(ctxFor(anonymous)), ["authorized (no user in response)"]);
  const refused = fakeServer({ "POST /api/authorize": () => new Response("no", { status: 401 }) });
  const report = await guest.health(ctxFor(refused));
  assert.equal(report.ok, false);
  assert.match(report.detail, /HTTP 401/);
});

test("a user created while the plugin slept is reported after the next start", async () => {
  for (const profile of TEST_DATABASE_PROFILES) {
    const db = await createTestDatabase({ profile, migrations: migrationsFor("LISTENING_STATE") });
    try {
      const before = usersServer(["root", "ada"]);
      await guest.start(guest.newState(), ctxFor(before, { db: db.binding }));
      assert.deepEqual(
        await guest.pollEvents(guest.newState(), ctxFor(before, { db: db.binding })),
        [],
      );

      // A new isolate: memory is empty, the database is not.
      const after = usersServer(["root", "ada", "grace"]);
      const state = guest.newState();
      await guest.start(state, ctxFor(after, { db: db.binding }));
      assert.deepEqual(await guest.pollEvents(state, ctxFor(after, { db: db.binding })), [
        { provider: "audiobookshelf", externalUserId: "grace", displayName: "grace-name" },
      ]);
      assert.deepEqual(
        await guest.pollEvents(state, ctxFor(after, { db: db.binding })),
        [],
        profile,
      );
    } finally {
      await db.close();
    }
  }
});

test("a poll seeds the watch when start could not, and reports nobody", async () => {
  const state = guest.newState();
  const down = fakeServer({ "GET /api/users": () => new Response("", { status: 503 }) });
  await guest.start(state, ctxFor(down));
  const up = usersServer(["root"]);
  assert.deepEqual(await guest.pollEvents(state, ctxFor(up)), []);
  const later = usersServer(["root", "ada"]);
  assert.deepEqual(
    (await guest.pollEvents(state, ctxFor(later))).map((u) => u.externalUserId),
    ["ada"],
  );
});

test("without a database, a seed of an empty server still counts", async () => {
  const state = guest.newState();
  await guest.start(state, ctxFor(usersServer([])));
  assert.deepEqual(
    (await guest.pollEvents(state, ctxFor(usersServer(["ada"])))).map((u) => u.externalUserId),
    ["ada"],
  );
});

test("with the watch off, start and poll ask the server nothing", async () => {
  const server = usersServer(["root"]);
  const sockets = fakeSockets();
  const state = guest.newState();
  await guest.start(state, ctxFor(server, { watchUsers: false, sockets: sockets.binding }));
  assert.deepEqual(
    await guest.pollEvents(state, ctxFor(server, { watchUsers: false, sockets: sockets.binding })),
    [],
  );
  assert.equal(server.seen.length, 0);
  assert.equal(sockets.connects.length, 0);
});

test("start needs a key", async () => {
  await assert.rejects(
    guest.start(guest.newState(), ctxFor(usersServer([]), { apiKey: null })),
    (err: unknown) => err instanceof AbsFailure && err.code === "invalid_params",
  );
});

test("the pushed-event socket opens once, with the host answering pings", async () => {
  const sockets = fakeSockets();
  const ctx = ctxFor(usersServer([]), { sockets: sockets.binding });
  assert.equal(await guest.ensurePush(ctx), true);
  assert.equal(await guest.ensurePush(ctx), false, "a socket is already open");
  assert.deepEqual(sockets.connects, [
    {
      url: "wss://abs.example/socket.io/?EIO=4&transport=websocket",
      options: { autoResponses: [{ request: "2", response: "3" }] },
    },
  ]);
  sockets.open = 0;
  await guest.pollEvents(guest.newState(), ctx);
  assert.equal(sockets.connects.length, 2, "a poll reopens a closed socket");
});

test("no socket without the binding, the watch, a key or a usable URL", async () => {
  const sockets = fakeSockets();
  const server = usersServer([]);
  assert.equal(await guest.ensurePush(ctxFor(server)), false);
  assert.equal(
    await guest.ensurePush(ctxFor(server, { sockets: sockets.binding, watchUsers: false })),
    false,
  );
  assert.equal(
    await guest.ensurePush(ctxFor(server, { sockets: sockets.binding, apiKey: null })),
    false,
  );
  assert.equal(
    await guest.ensurePush(
      ctxFor(server, { sockets: sockets.binding, baseUrl: "http://abs.example" }),
    ),
    false,
  );
  assert.equal(sockets.connects.length, 0);
});

test("a socket that cannot open does not fail start or poll", async () => {
  const broken = {
    connect: () => Promise.reject(new Error("denied")),
    getWebSockets: () => Promise.resolve([]),
  } as unknown as SocketsBinding;
  const server = usersServer(["root"]);
  const state = guest.newState();
  await guest.start(state, ctxFor(server, { sockets: broken }));
  assert.deepEqual(await guest.pollEvents(state, ctxFor(server, { sockets: broken })), []);
});

test("the socket handshake joins, authenticates, and closes on a rejected key", async () => {
  const ctx = ctxFor(usersServer([]));
  const socket = fakeSocket();
  assert.equal(await guest.pushFrame(ctx, socket.ws, '0{"sid":"x"}'), false);
  assert.equal(await guest.pushFrame(ctx, socket.ws, '40{"sid":"y"}'), false);
  assert.equal(await guest.pushFrame(ctx, socket.ws, '42["user_added",{"id":"u9"}]'), true);
  assert.equal(await guest.pushFrame(ctx, socket.ws, new ArrayBuffer(2)), false);
  assert.deepEqual(socket.sent, ["40", '42["auth","secret-key"]']);
  assert.deepEqual(socket.closed, []);
  assert.equal(await guest.pushFrame(ctx, socket.ws, '42["invalid_token"]'), false);
  assert.deepEqual(socket.closed, [1000]);
});

test("book_acquired asks for a rescan; other events and switches only acknowledge", async () => {
  const server = fakeServer({
    "POST /api/libraries/lib1/scan": () => new Response(null, { status: 200 }),
  });
  assert.deepEqual(await guest.onEvent(ctxFor(server), "book_acquired"), { kind: "ack" });
  assert.deepEqual(await guest.onEvent(ctxFor(server), "work_updated"), { kind: "ack" });
  assert.deepEqual(
    await guest.onEvent(ctxFor(server, { notifyScanOnAcquire: false }), "book_acquired"),
    {
      kind: "ack",
    },
  );
  assert.deepEqual(await guest.onEvent(ctxFor(server, { libraryId: null }), "book_acquired"), {
    kind: "ack",
  });
  assert.deepEqual(
    server.seen.map((s) => `${s.method} ${s.path}`),
    ["POST /api/libraries/lib1/scan"],
  );
});

test("a failed or misconfigured rescan is retried", async () => {
  const down = fakeServer({
    "POST /api/libraries/lib1/scan": () => new Response("busy", { status: 503 }),
  });
  const failed = await guest.onEvent(ctxFor(down), "book_acquired");
  assert.equal(failed.kind, "retry");
  const missing = await guest.onEvent(ctxFor(down, { apiKey: null }), "book_acquired");
  assert.deepEqual(missing, { kind: "retry", reason: guest.MISSING_KEY });
});

test("scanLibrary needs a library id and passes force", async () => {
  const server = fakeServer({
    "POST /api/libraries/lib1/scan?force=1": () => new Response(null, { status: 204 }),
  });
  await guest.scanLibrary(ctxFor(server), true);
  await assert.rejects(
    guest.scanLibrary(ctxFor(server, { libraryId: null }), false),
    (err: unknown) => err instanceof AbsFailure && err.code === "invalid_params",
  );
});

test("syncListening returns the changed rows", async () => {
  const server = fakeServer((seen) => {
    if (seen.path === "/api/users") return json({ users: [{ id: "u1" }] });
    if (seen.path === "/api/users/u1")
      return json({ id: "u1", mediaProgress: [{ libraryItemId: "a", progress: 1 }] });
    if (seen.path === "/api/items/a") return json({ media: { metadata: { title: "A" } } });
    return undefined;
  });
  const rows = await guest.syncListening(guest.newState(), ctxFor(server), 1_800_000_000_000);
  assert.deepEqual(rows, [
    { externalUserId: "u1", externalItemId: "a", isFinished: false, title: "A", progress: 1 },
  ]);
});

test("credential sign-in can be turned off", async () => {
  const server = fakeServer({
    "POST /login": () => json({ user: { id: "u1", username: "", token: null } }),
  });
  assert.deepEqual(await guest.authenticateUser(ctxFor(server), "bob", "pw"), {
    provider: "audiobookshelf",
    externalUserId: "u1",
  });
  await assert.rejects(
    guest.authenticateUser(ctxFor(server, { allowCredentialLogin: false }), "bob", "pw"),
    (err: unknown) => err instanceof AbsFailure && err.code === "forbidden",
  );
  assert.equal(server.seen.length, 1);
});

test("serialized calls run one at a time, even after a failure", async () => {
  const state = guest.newState();
  const order: string[] = [];
  const first = guest.serialized(state, async () => {
    await new Promise((resolve) => setTimeout(resolve, 5));
    order.push("first");
    throw new Error("boom");
  });
  const second = guest.serialized(state, () => {
    order.push("second");
    return Promise.resolve(2);
  });
  await assert.rejects(first, /boom/);
  assert.equal(await second, 2);
  assert.deepEqual(order, ["first", "second"]);
});
