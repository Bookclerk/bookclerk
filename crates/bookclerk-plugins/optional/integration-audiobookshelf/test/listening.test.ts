/**
 * The listening sync and its memory, against a fake server and the
 * binding a host grants, under every `bookclerk-db-testkit` profile.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  TEST_DATABASE_PROFILES,
  createTestDatabase,
  type TestDatabase,
} from "@bookclerk/plugin-sdk/testkit";
import type { DatabaseBinding } from "@bookclerk/plugin-sdk/workerd";

import { AbsClient, RequestBudget } from "../src/client.ts";
import {
  FULL_RESEND_MS,
  LISTENING_MIGRATION,
  RETENTION_MS,
  collectListeningChanges,
  hostRow,
  migrationsFor,
} from "../src/listening.ts";
import { WATCH_MIGRATION } from "../src/watch.ts";
import { fakeServer, json, type FakeServer } from "./support.ts";

/** One progress row on the fake server. */
interface Progress {
  /** Library item id. */
  readonly libraryItemId: string;
  /** Fraction listened. */
  progress: number;
  /** Whether finished. */
  readonly isFinished?: boolean;
}

/** A fake server with users, their progress rows and item metadata. */
interface Library {
  /** The server. */
  readonly server: FakeServer;
  /** Progress rows by user id; edit to change what the next sync sees. */
  readonly users: Map<string, Progress[]>;
}

/**
 * A server whose users hold `rows` each.
 *
 * @param users - Progress rows by user id.
 * @returns The library.
 */
function library(users: Record<string, Progress[]>): Library {
  const map = new Map(Object.entries(users));
  const server = fakeServer((seen) => {
    if (seen.path === "/api/users") {
      return json({ users: [...map.keys()].map((id) => ({ id, username: id })) });
    }
    const user = /^\/api\/users\/([^/]+)$/.exec(seen.path);
    if (user !== null) {
      const id = decodeURIComponent(user[1] ?? "");
      const rows = map.get(id);
      return rows === undefined ? undefined : json({ id, mediaProgress: rows });
    }
    const item = /^\/api\/items\/([^/]+)$/.exec(seen.path);
    if (item !== null) {
      const id = decodeURIComponent(item[1] ?? "");
      return json({
        media: { metadata: { title: `Title ${id}`, authorName: "Author", asin: "b0abc" } },
      });
    }
    return undefined;
  });
  return { server, users: map };
}

/**
 * A client for `lib` with `budget` requests.
 *
 * @param lib - The library.
 * @param budget - Requests allowed.
 * @returns The client.
 */
function client(lib: Library, budget = 990): AbsClient {
  return new AbsClient(lib.server.baseUrl, "k", lib.server.fetch, new RequestBudget(budget));
}

/**
 * Item lookups the server has seen.
 *
 * @param lib - The library.
 * @returns How many.
 */
function lookups(lib: Library): number {
  return lib.server.seen.filter((s) => s.path.startsWith("/api/items/")).length;
}

/**
 * Runs `body` against a fresh migrated database under every profile.
 *
 * @param body - The checks.
 */
async function eachProfile(
  body: (db: TestDatabase, profile: string) => Promise<void>,
): Promise<void> {
  for (const profile of TEST_DATABASE_PROFILES) {
    const db = await createTestDatabase({
      profile,
      migrations: [LISTENING_MIGRATION, WATCH_MIGRATION],
    });
    try {
      await body(db, profile);
    } finally {
      await db.close();
    }
  }
}

/** A fixed sync time. */
const T0 = 1_800_000_000_000;

test("migrations belong to the LISTENING_STATE binding only", () => {
  assert.deepEqual(
    migrationsFor("LISTENING_STATE").map((m) => m.id),
    ["abs-listening-v1", "abs-user-watch-v1"],
  );
  assert.deepEqual(migrationsFor("OTHER"), []);
});

test("a host row omits what is unknown and upper-cases the ASIN", () => {
  const row = {
    libraryItemId: "li",
    progress: null,
    currentTime: 3,
    duration: null,
    isFinished: false,
    lastUpdate: -1,
  };
  assert.deepEqual(hostRow("u", row, undefined), {
    externalUserId: "u",
    externalItemId: "li",
    isFinished: false,
    currentTimeSeconds: 3,
  });
  assert.deepEqual(
    hostRow(
      "u",
      { ...row, lastUpdate: 5 },
      { title: "T", authorName: null, asin: "b0x", isbn: "978" },
    ),
    {
      externalUserId: "u",
      externalItemId: "li",
      isFinished: false,
      title: "T",
      asin: "B0X",
      isbn: "978",
      currentTimeSeconds: 3,
      lastListenedAtUnixMs: 5,
    },
  );
});

test("a second sync sends only the rows that changed", async () => {
  await eachProfile(async (db, profile) => {
    const lib = library({
      u1: [
        { libraryItemId: "a", progress: 0.1 },
        { libraryItemId: "b", progress: 0.2 },
      ],
      u2: [{ libraryItemId: "a", progress: 0.5 }],
    });
    const first = await collectListeningChanges(client(lib), db.binding, T0);
    assert.equal(first.complete, true);
    assert.equal(first.rows.length, 3, profile);
    assert.equal(lookups(lib), 2, `${profile}: one lookup per item, shared across users`);
    assert.equal(first.rows[0]?.title, "Title a");
    assert.equal(first.rows[0]?.asin, "B0ABC");

    const second = await collectListeningChanges(client(lib), db.binding, T0 + 1000);
    assert.deepEqual(second.rows, [], `${profile}: nothing changed`);

    const u1 = lib.users.get("u1") ?? [];
    (u1[1] as Progress).progress = 0.9;
    const third = await collectListeningChanges(client(lib), db.binding, T0 + 2000);
    assert.deepEqual(
      third.rows.map((r) => [r.externalUserId, r.externalItemId, r.progress]),
      [["u1", "b", 0.9]],
      profile,
    );
    assert.deepEqual((await collectListeningChanges(client(lib), db.binding, T0 + 3000)).rows, []);
  });
});

test("every row is sent again once a day", async () => {
  await eachProfile(async (db, profile) => {
    const lib = library({ u1: [{ libraryItemId: "a", progress: 0.1 }] });
    await collectListeningChanges(client(lib), db.binding, T0);
    assert.deepEqual(
      (await collectListeningChanges(client(lib), db.binding, T0 + FULL_RESEND_MS - 1)).rows,
      [],
    );
    const resent = await collectListeningChanges(client(lib), db.binding, T0 + FULL_RESEND_MS);
    assert.equal(resent.rows.length, 1, profile);
    assert.deepEqual(
      (await collectListeningChanges(client(lib), db.binding, T0 + FULL_RESEND_MS + 1)).rows,
      [],
    );
  });
});

test("many rows are remembered across paged reads", async () => {
  await eachProfile(async (db, profile) => {
    const rows = Array.from({ length: 300 }, (_, i) => ({
      libraryItemId: `item-${i}`,
      progress: 0.5,
    }));
    const lib = library({ u1: rows });
    assert.equal((await collectListeningChanges(client(lib), db.binding, T0)).rows.length, 300);
    assert.deepEqual(
      (await collectListeningChanges(client(lib), db.binding, T0 + 1)).rows,
      [],
      profile,
    );
  });
});

test("users not synced for a week are forgotten", async () => {
  await eachProfile(async (db, profile) => {
    const lib = library({ u1: [{ libraryItemId: "a", progress: 0.1 }] });
    await collectListeningChanges(client(lib), db.binding, T0);
    const gone = library({});
    await collectListeningChanges(client(gone), db.binding, T0 + RETENTION_MS + 1);
    const users = await db.binding.prepare("SELECT person FROM abs_listening_users").all();
    const items = await db.binding.prepare("SELECT item FROM abs_listening_items").all();
    assert.equal(users.results?.length ?? 0, 0, profile);
    assert.equal(items.results?.length ?? 0, 0, profile);
  });
});

test("a budget that runs out leaves the rest for the next sync", async () => {
  await eachProfile(async (db, profile) => {
    const lib = library({
      u1: [
        { libraryItemId: "a", progress: 0.1 },
        { libraryItemId: "b", progress: 0.2 },
        { libraryItemId: "c", progress: 0.3 },
      ],
      u2: [{ libraryItemId: "d", progress: 0.4 }],
    });
    // users list, u1, then two item lookups.
    const short = await collectListeningChanges(client(lib, 4), db.binding, T0);
    assert.equal(short.complete, false, profile);
    assert.deepEqual(
      short.rows.map((r) => r.externalItemId),
      ["a", "b"],
      profile,
    );
    const rest = await collectListeningChanges(client(lib), db.binding, T0 + 1);
    assert.equal(rest.complete, true);
    assert.deepEqual(
      rest.rows.map((r) => r.externalItemId),
      ["c", "d"],
      profile,
    );
  });
});

test("a memory that fails never fails the sync", async () => {
  // A database without the tables: every memory call fails.
  for (const profile of TEST_DATABASE_PROFILES) {
    const db = await createTestDatabase({ profile, migrations: [] });
    try {
      const lib = library({ u1: [{ libraryItemId: "a", progress: 0.1 }] });
      assert.equal(
        (await collectListeningChanges(client(lib), db.binding, T0)).rows.length,
        1,
        profile,
      );
      assert.equal(
        (await collectListeningChanges(client(lib), db.binding, T0 + 1)).rows.length,
        1,
        profile,
      );
    } finally {
      await db.close();
    }
  }
});

test("without a database every row is sent", async () => {
  const lib = library({ u1: [{ libraryItemId: "a", progress: 0.1 }] });
  const none: DatabaseBinding | undefined = undefined;
  assert.equal((await collectListeningChanges(client(lib), none, T0)).rows.length, 1);
  assert.equal((await collectListeningChanges(client(lib), none, T0 + 1)).rows.length, 1);
});

test("a user the server cannot return is skipped and others still sync", async () => {
  const lib = library({ u1: [{ libraryItemId: "a", progress: 0.1 }] });
  const server = fakeServer((seen) => {
    if (seen.path === "/api/users") return json({ users: [{ id: "ghost" }, { id: "u1" }] });
    if (seen.path === "/api/users/ghost") return new Response("gone", { status: 404 });
    return undefined;
  });
  const merged: typeof server.fetch = (url, init) =>
    url.endsWith("/api/users") || url.endsWith("/ghost")
      ? server.fetch(url, init)
      : lib.server.fetch(url, init);
  const sync = await collectListeningChanges(
    new AbsClient(server.baseUrl, "k", merged),
    undefined,
    T0,
  );
  assert.deepEqual(
    sync.rows.map((r) => r.externalUserId),
    ["u1"],
  );
  assert.equal(sync.complete, true);
});
