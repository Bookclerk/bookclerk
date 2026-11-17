/**
 * The user watch against the binding a host grants, under every
 * `bookclerk-db-testkit` profile, and in memory.
 *
 * The kit binary comes from `BOOKCLERK_DB_TESTKIT` (CI builds it), else
 * `bookclerk-db-testkit` on `PATH`.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  TEST_DATABASE_PROFILES,
  createTestDatabase,
  type TestDatabase,
} from "@bookclerk/plugin-sdk/testkit";
import type { PluginMigration } from "@bookclerk/plugin-sdk/workerd";

import type { AbsUser } from "../src/client.ts";
import { hexKey, LISTENING_KEY_TAG } from "../src/hash.ts";
import { SEEDED, UserWatch, WATCH_MIGRATION, insertKeys, watchKey } from "../src/watch.ts";

/**
 * Users with the given ids.
 *
 * @param ids - User ids.
 * @returns The users.
 */
function users(ids: readonly string[]): AbsUser[] {
  return ids.map((id) => ({ id, username: id, token: null }));
}

/**
 * The ids of `list`.
 *
 * @param list - Users.
 * @returns Their ids.
 */
function ids(list: readonly AbsUser[]): string[] {
  return list.map((user) => user.id);
}

/**
 * Runs `body` against a fresh database under every profile.
 *
 * @param migrations - The registration to apply.
 * @param body - The checks.
 */
async function eachProfile(
  migrations: PluginMigration[],
  body: (db: TestDatabase, profile: string) => Promise<void>,
): Promise<void> {
  for (const profile of TEST_DATABASE_PROFILES) {
    const db = await createTestDatabase({ profile, migrations });
    try {
      await body(db, profile);
    } finally {
      await db.close();
    }
  }
}

/** 300 user ids, enough to span several pages and batches on the small profiles. */
const MANY = Array.from({ length: 300 }, (_, i) => `user-${i}`);

test("keys match what earlier releases stored, and hide the id", async () => {
  // Independent oracle: Python's hashlib over the same tag, NUL and id.
  assert.equal(await watchKey("8f2c-user"), "d71484989bd4afcc");
  assert.equal(await watchKey("root"), "a687b45be61928f4");
  assert.equal(await hexKey(LISTENING_KEY_TAG, "u1"), "8e037e7f14fa9b47");
  assert.notEqual(await watchKey("other"), await watchKey("8f2c-user"));
  assert.ok(SEEDED < "0000000000000000", "the marker sorts before every key");
});

test("the database remembers reported users across plugin starts", async () => {
  await eachProfile([WATCH_MIGRATION], async (db, profile) => {
    const firstMemory = new Set<string>();
    const first = new UserWatch(db.binding, firstMemory);
    assert.equal(await first.seed(users(["root", "ada"])), 2);
    assert.equal(firstMemory.size, 0, "the database holds the set");

    const later = new UserWatch(db.binding, new Set());
    assert.equal(await later.seed(users(["root"])), 0);
    const fresh = await later.takeNew(users(["root", "ada", "grace", "grace"]));
    assert.deepEqual(ids(fresh), ["grace"], profile);
    assert.deepEqual(await later.takeNew(users(["root", "ada", "grace"])), []);
  });
});

test("many users are read back across paged reads", async () => {
  await eachProfile([WATCH_MIGRATION], async (db, profile) => {
    const watch = new UserWatch(db.binding, new Set());
    assert.equal(await watch.seed(users(MANY)), 300);
    const fresh = await watch.takeNew(users([...MANY, "newcomer"]));
    assert.deepEqual(ids(fresh), ["newcomer"], profile);
  });
});

test("a seed cut short is redone in full", async () => {
  await eachProfile([WATCH_MIGRATION], async (db, profile) => {
    // A seed that wrote one user row and stopped before its marker.
    await insertKeys(db.binding, [await watchKey("root")]);
    const watch = new UserWatch(db.binding, new Set());
    assert.equal(await watch.seed(users(["root", "ada"])), 2);
    assert.equal(await watch.seed(users(["root", "ada"])), 0);
    assert.deepEqual(
      await watch.takeNew(users(["root", "ada"])),
      [],
      `${profile}: a baseline user was reported`,
    );
  });
});

test("new users are reported even when recording them fails", async () => {
  const root = await watchKey("root");
  const strict: PluginMigration = {
    id: "abs-user-watch-strict",
    operations: [
      {
        kind: "schema",
        value: `CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY CHECK (person IN ('${SEEDED}', '${root}')))`,
      },
    ],
  };
  await eachProfile([strict], async (db, profile) => {
    const watch = new UserWatch(db.binding, new Set());
    assert.equal(await watch.seed(users(["root"])), 1);
    assert.deepEqual(ids(await watch.takeNew(users(["root", "ada"]))), ["ada"], profile);
    assert.deepEqual(
      ids(await watch.takeNew(users(["root", "ada"]))),
      ["ada"],
      `${profile}: an unrecorded user is reported again`,
    );
  });
});

test("a later batch failing still reports every new user", async () => {
  const last = await watchKey("user-299");
  const strict: PluginMigration = {
    id: "abs-user-watch-strict-batches",
    operations: [
      {
        kind: "schema",
        value: `CREATE TABLE IF NOT EXISTS abs_watch_users (person TEXT NOT NULL PRIMARY KEY CHECK (person <> '${last}'))`,
      },
    ],
  };
  await eachProfile([strict], async (db, profile) => {
    const watch = new UserWatch(db.binding, new Set());
    assert.equal(await watch.seed(users(["root"])), 1);
    const polled = users(["root", ...MANY]);
    assert.deepEqual(
      ids(await watch.takeNew(polled)),
      MANY,
      `${profile}: every new user is reported`,
    );
    const again = ids(await watch.takeNew(polled));
    assert.ok(
      again.includes("user-299"),
      `${profile}: the user whose batch failed is reported again`,
    );
    assert.ok(
      again.every((id) => MANY.includes(id)),
      `${profile}: a baseline user was reported`,
    );
    if (profile === "minimum") {
      assert.ok(again.length < MANY.length, "the smallest profile commits the earlier batches");
    }
  });
});

test("without a database the set lives in memory", async () => {
  const memory = new Set<string>();
  const watch = new UserWatch(undefined, memory);
  assert.equal(await watch.seed(users(["root"])), 1);
  assert.equal(await watch.seed(users(["root", "ada"])), 0);
  assert.deepEqual(ids(await watch.takeNew(users(["root", "ada"]))), ["ada"]);
  assert.equal(memory.size, 2);
});
