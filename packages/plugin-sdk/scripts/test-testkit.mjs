#!/usr/bin/env node
/**
 * `createTestDatabase({ profile })`: the served binding behaves like a host
 * grant under every profile.
 *
 * Needs the `bookclerk-db-testkit` binary: `BOOKCLERK_DB_TESTKIT` or
 * `bookclerk-db-testkit` on `PATH`; with neither, the script builds it with
 * `cargo build -p bookclerk-db-testkit` from the workspace root.
 */
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");
const workspace = join(root, "../../..");

const { TEST_DATABASE_PROFILES, createTestDatabase, defaultProfile, kitMigrations, testkitBinary } =
  await import(join(dist, "testkit.js"));
const { PluginError } = await import(join(dist, "errors.js"));

function ensureBinary() {
  if (
    process.env.BOOKCLERK_DB_TESTKIT ||
    spawnSync("bookclerk-db-testkit", ["--help"]).status === 0
  ) {
    return;
  }
  const built = join(
    workspace,
    "target/debug",
    process.platform === "win32" ? "bookclerk-db-testkit.exe" : "bookclerk-db-testkit",
  );
  if (!existsSync(built)) {
    const r = spawnSync(
      "cargo",
      ["build", "-p", "bookclerk-db-testkit", "--bin", "bookclerk-db-testkit"],
      {
        cwd: workspace,
        stdio: "inherit",
      },
    );
    assert.equal(r.status, 0, "cargo build -p bookclerk-db-testkit");
  }
  process.env.BOOKCLERK_DB_TESTKIT = built;
}

ensureBinary();
assert.equal(testkitBinary("explicit"), "explicit");
assert.equal(testkitBinary(), process.env.BOOKCLERK_DB_TESTKIT ?? "bookclerk-db-testkit");

const MIGRATIONS = [
  {
    id: "notes-v1",
    operations: [
      {
        schema:
          "CREATE TABLE IF NOT EXISTS notes (id INTEGER PRIMARY KEY NOT NULL, body TEXT NOT NULL)",
      },
    ],
  },
];

assert.deepEqual(TEST_DATABASE_PROFILES, ["first-party", "d1", "minimum"]);

// Typed binding values, as a plugin passes them.
const i64 = (n) => ({ kind: "int64", value: BigInt(n) });
const text = (s) => ({ kind: "text", value: s });

// --- Every profile: schema applied, writes and reads through the binding ---
for (const profile of TEST_DATABASE_PROFILES) {
  const db = await createTestDatabase({ profile, migrations: MIGRATIONS });
  try {
    assert.equal(db.profile, profile);
    const limits = await db.limits();
    assert.equal(limits.sqlContractVersion, 1);
    assert.ok(limits.maxCellBytes >= 1024, `${profile}: maxCellBytes ${limits.maxCellBytes}`);

    const run = await db.binding
      .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
      .bind(i64(1), text("first"))
      .run();
    assert.equal(run.success, true);
    const first = await db.binding
      .prepare("SELECT id, body FROM notes WHERE id = ?")
      .bind(i64(1))
      .first();
    assert.deepEqual(first, { id: i64(1), body: text("first") });

    // A batch is one transaction: the repeated key rolls the whole batch back.
    await assert.rejects(
      db.binding.batch([
        db.binding
          .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
          .bind(i64(2), text("second")),
        db.binding
          .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
          .bind(i64(1), text("again")),
      ]),
      (err) => err instanceof PluginError && err.code !== "invalid_params",
    );
    const { results } = await db.binding.prepare("SELECT id FROM notes ORDER BY id").all();
    assert.deepEqual(results, [{ id: i64(1) }]);

    // The binding reaches only its own tables.
    await assert.rejects(
      db.binding.prepare("SELECT operation_id FROM bookclerk_receipts").all(),
      (err) => err instanceof PluginError && err.code === "invalid_params",
    );
  } finally {
    await db.close();
  }
  console.log("ok", profile, "schema, batch, scope");
}

// --- Minimum: a result cell past the cap is the typed error ------------------
{
  const db = await createTestDatabase({ migrations: MIGRATIONS });
  try {
    assert.equal(db.profile, "minimum");
    const limits = await db.limits();
    const body = "x".repeat(limits.maxCellBytes / 2 + 1);
    await db.binding
      .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
      .bind(i64(1), text(body))
      .run();
    await assert.rejects(
      db.binding
        .prepare("SELECT body || body AS twice FROM notes WHERE id = ?")
        .bind(i64(1))
        .first(),
      (err) =>
        err instanceof PluginError &&
        err.code === "limit_exceeded" &&
        err.limit?.limit === "maxCellBytes" &&
        err.limit?.cap === limits.maxCellBytes,
    );
    // `all()` cannot stream under minimum; the inline cap still answers typed.
    const wide = Array.from({ length: 8 }, (_, i) =>
      db.binding
        .prepare("INSERT INTO notes (id, body) VALUES (?, ?)")
        .bind(i64(10 + i), text(body)),
    );
    for (const stmt of wide) await stmt.run();
    await assert.rejects(
      db.binding.prepare("SELECT id, body FROM notes").all(),
      (err) => err instanceof PluginError && err.code === "limit_exceeded",
    );
  } finally {
    await db.close();
  }
  console.log("ok minimum typed limits");
}

// --- First-party: all() streams a result past the inline cap ----------------
{
  const db = await createTestDatabase({ profile: "first-party", migrations: MIGRATIONS });
  try {
    const limits = await db.limits();
    assert.ok(limits.maxStreamedResultBytes > limits.maxResultBytes);
    const body = "y".repeat(16 * 1024);
    const inserts = Array.from({ length: 40 }, (_, i) =>
      db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)").bind(i64(i), text(body)),
    );
    for (let at = 0; at < inserts.length; at += 10) {
      await db.binding.batch(inserts.slice(at, at + 10));
    }
    const { results } = await db.binding.prepare("SELECT id, body FROM notes ORDER BY id").all();
    assert.equal(results.length, 40);
    assert.ok(
      results.length * body.length > limits.maxResultBytes,
      "the result exceeds the inline cap",
    );
  } finally {
    await db.close();
  }
  console.log("ok first-party streamed all()");
}

// --- Default profile ------------------------------------------------------------
{
  const saved = process.env.BOOKCLERK_DB_TESTKIT_PROFILE;
  delete process.env.BOOKCLERK_DB_TESTKIT_PROFILE;
  assert.equal(defaultProfile(undefined), "minimum");
  process.env.BOOKCLERK_DB_TESTKIT_PROFILE = "";
  assert.equal(defaultProfile(undefined), "minimum");
  process.env.BOOKCLERK_DB_TESTKIT_PROFILE = "first-party";
  assert.equal(defaultProfile(undefined), "first-party");
  assert.equal(defaultProfile("d1"), "d1");
  // `bookclerk-plugin test` names the profile; the wrapper runs under it.
  const fromEnv = await createTestDatabase();
  try {
    assert.equal(fromEnv.profile, "first-party");
  } finally {
    await fromEnv.close();
  }
  process.env.BOOKCLERK_DB_TESTKIT_PROFILE = "huge";
  await assert.rejects(createTestDatabase(), /unknown test database profile "huge"/);
  if (saved === undefined) delete process.env.BOOKCLERK_DB_TESTKIT_PROFILE;
  else process.env.BOOKCLERK_DB_TESTKIT_PROFILE = saved;
  console.log("ok default profile");
}

// --- Refusals -----------------------------------------------------------------
await assert.rejects(createTestDatabase({ profile: "huge" }), /unknown test database profile/);
await assert.rejects(
  createTestDatabase({
    migrations: [
      {
        id: "bad",
        operations: [{ schema: "CREATE TABLE IF NOT EXISTS bookclerk_receipts (x INTEGER)" }],
      },
    ],
  }),
  // The kit's own refusal is part of the message, not only the exit.
  /bookclerk-db-testkit exited before announcing its address: .*bookclerk_receipts/s,
);
assert.throws(
  () => kitMigrations([{ id: "odd", operations: [{ kind: "rename", value: "x" }] }]),
  /migration "odd" has an operation without schema or data/,
);
// A registration the SDK refuses is refused before any kit starts: the
// binary does not exist, so the spawn error would win if it ran first.
await assert.rejects(
  createTestDatabase({
    binary: join(root, "no-such-bookclerk-db-testkit"),
    migrations: [{ id: "odd", operations: [{ kind: "rename", value: "x" }] }],
  }),
  /migration "odd" has an operation without schema or data/,
);
await assert.rejects(
  createTestDatabase({ binary: join(root, "no-such-bookclerk-db-testkit") }),
  (err) => err instanceof Error,
);
console.log("ok refusals");

// --- Typed migration operations, as `PluginMigration` spells them ------------
assert.deepEqual(
  kitMigrations([
    {
      id: "m",
      operations: [
        { kind: "schema", value: "CREATE TABLE t (x INTEGER)" },
        { kind: "data", value: "INSERT INTO t (x) VALUES (1)" },
        { schema: "CREATE TABLE u (y INTEGER)" },
        { data: "INSERT INTO u (y) VALUES (2)" },
      ],
    },
  ]),
  [
    {
      id: "m",
      operations: [
        { schema: "CREATE TABLE t (x INTEGER)" },
        { data: "INSERT INTO t (x) VALUES (1)" },
        { schema: "CREATE TABLE u (y INTEGER)" },
        { data: "INSERT INTO u (y) VALUES (2)" },
      ],
    },
  ],
);
{
  const db = await createTestDatabase({
    migrations: [
      {
        id: "typed-v1",
        operations: [
          {
            kind: "schema",
            value:
              "CREATE TABLE IF NOT EXISTS typed (id INTEGER PRIMARY KEY NOT NULL, v TEXT NOT NULL)",
          },
          { kind: "data", value: "INSERT INTO typed (id, v) VALUES (1, 'seeded')" },
        ],
      },
    ],
  });
  try {
    const row = await db.binding.prepare("SELECT v FROM typed WHERE id = ?").bind(i64(1)).first();
    assert.deepEqual(row, { v: text("seeded") });
  } finally {
    await db.close();
  }
}
console.log("ok typed migrations");
