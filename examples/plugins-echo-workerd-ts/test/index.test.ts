/**
 * The entrypoint glue: CLI commands and the migrations hook, with the SDK's
 * isolate module replaced by stand-ins (the real one imports
 * `cloudflare:workers`, which only exists inside workerd) and `env.DB`
 * served by `bookclerk-db-testkit` under the profile `bookclerk-plugin test`
 * names.
 */

import assert from "node:assert/strict";
import { after, before, mock, test } from "node:test";

import { createTestDatabase, type TestDatabase } from "@bookclerk/plugin-sdk/testkit";

import type { CliInvokeParams } from "@bookclerk/plugin-sdk/workerd";

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

/**
 * Stand-in for the SDK's `cliArgs`: argument name to value.
 *
 * @param params - Invocation parameters.
 * @returns The arguments by name.
 */
function cliArgs(params: Pick<CliInvokeParams, "args">): Record<string, string> {
  return Object.fromEntries((params.args ?? []).map((a) => [a.name, String(a.value ?? "")]));
}

/**
 * Stand-in for the SDK's `jsonPayload`: keeps the value readable.
 *
 * @param value - Payload value.
 * @returns The value, unchanged.
 */
function jsonPayload(value: unknown): unknown {
  return value;
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  namedExports: {
    BookclerkEntrypoint: Entrypoint,
    CliEntrypoint: Entrypoint,
    cliArgs,
    jsonPayload,
  },
});
const { Cli, default: Plugin } = await import("../src/index.ts");
const { MIGRATIONS } = await import("../src/notes.ts");

/** The served binding. */
let db: TestDatabase;
/** The `cli` entrypoint over that binding. */
let cli: InstanceType<typeof Cli>;

before(async () => {
  db = await createTestDatabase({ migrations: MIGRATIONS });
  cli = new Cli({} as never, { DB: db.binding } as never);
});

after(async () => {
  await db.close();
});

/**
 * Invokes one CLI command.
 *
 * @param command - Command name.
 * @param args - Arguments by name.
 * @returns The reply.
 */
function run(command: string, args: Record<string, string> = {}) {
  const params = {
    command,
    args: Object.entries(args).map(([name, value]) => ({ name, value })),
  } as unknown as CliInvokeParams;
  return cli.invoke(params);
}

test("the CLI schema lists every command", async () => {
  const schema = await cli.describe();
  assert.deepEqual(
    schema.commands.map((c) => c.name),
    ["ping", "notes-add", "notes-list"],
  );
});

test("ping echoes its message", async () => {
  const r = await run("ping", { message: "hello" });
  assert.equal(r.exitCode, 0);
  assert.equal(r.stdout, "pong: hello\n");
});

test("notes-add stores the note COUNT times and notes-list prints them", async () => {
  const added = await run("notes-add", { body: "remember", count: "3" });
  assert.equal(added.exitCode, 0, added.stderr);
  assert.match(added.stdout, /^stored 3; \d+ in total\n$/);
  const listed = await run("notes-list");
  assert.equal(listed.exitCode, 0, listed.stderr);
  const lines = listed.stdout.trimEnd().split("\n");
  assert.ok(lines.length >= 3);
  for (const line of lines.slice(-3)) assert.match(line, /^\d+: remember$/);
});

test("notes-add refuses a count out of range", async () => {
  for (const count of ["0", "-1", "1.5", "lots", "10001", "0x10", "1e3", " 5 ", ""]) {
    const r = await run("notes-add", { body: "x", count });
    assert.equal(r.exitCode, 2, count);
    assert.match(r.stderr, /--count must be 1 to 10000/);
  }
});

test("notes-list stops at --limit and refuses a limit out of range", async () => {
  await run("notes-add", { body: "many", count: "5" });
  const r = await run("notes-list", { limit: "2" });
  assert.equal(r.exitCode, 0, r.stderr);
  assert.equal(r.stdout.trimEnd().split("\n").length, 2);
  for (const limit of ["0", "1001", "two"]) {
    const bad = await run("notes-list", { limit });
    assert.equal(bad.exitCode, 2, limit);
    assert.match(bad.stderr, /--limit must be 1 to 1000/);
  }
});

test("notes-add says how many notes a failed write had already stored", async () => {
  const caps = await db.limits();
  const count = String(caps.maxStatements * 2);
  /**
   * A binding whose second batch fails with `failure`.
   *
   * @param failure - What the second `batch()` rejects with.
   * @returns The CLI over that binding.
   */
  const cliFailingWith = (failure: unknown) => {
    let batches = 0;
    const binding = new Proxy(db.binding, {
      get(target, key, receiver) {
        if (key === "batch") {
          return (...a: Parameters<typeof target.batch>) =>
            ++batches === 2 ? Promise.reject(failure) : target.batch(...a);
        }
        const value: unknown = Reflect.get(target, key, receiver);
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    return new Cli({} as never, { DB: binding } as never);
  };
  const limit = { code: "limit_exceeded", limit: { limit: "maxRequestBytes", cap: 1, used: 2 } };
  const params = (c: string) =>
    ({
      command: "notes-add",
      args: [
        { name: "body", value: "b" },
        { name: "count", value: c },
      ],
    }) as unknown as CliInvokeParams;
  const r = await cliFailingWith(limit).invoke(params(count));
  assert.equal(r.exitCode, 1);
  assert.match(
    r.stderr,
    /maxRequestBytes is 1, it needed 2\n\d+ notes were stored before the failure\n$/,
  );
  await assert.rejects(
    cliFailingWith(new Error("unavailable")).invoke(params(count)),
    /^Error: \d+ notes were stored before the failure: Error: unavailable$/,
  );
  const plain = new Error("unavailable");
  await assert.rejects(
    new Cli(
      {} as never,
      {
        DB: new Proxy(db.binding, {
          get(target, key, receiver) {
            if (key === "batch") return () => Promise.reject(plain);
            const value: unknown = Reflect.get(target, key, receiver);
            return typeof value === "function" ? value.bind(target) : value;
          },
        }),
      } as never,
    ).invoke(params("1")),
    (err) => err === plain,
  );
});

test("notes-add reports the cap a note is too large for", async () => {
  const caps = await db.limits();
  const r = await run("notes-add", {
    body: "x".repeat(Math.min(caps.maxCellBytes, caps.maxPayloadBytes) + 1),
  });
  assert.equal(r.exitCode, 1);
  assert.match(r.stderr, /^the database refused the request: max\w+ is \d+, it needed \d+\n$/);
});

test("an unknown command is a usage error", async () => {
  const r = await run("nope");
  assert.equal(r.exitCode, 2);
  assert.equal(r.stderr, "unknown command nope\n");
});

test("the migrations hook answers for the DB binding only", () => {
  const plugin = new Plugin({} as never, {} as never);
  assert.deepEqual(plugin.databaseMigrations("DB"), MIGRATIONS);
  assert.deepEqual(plugin.databaseMigrations("OTHER"), []);
});
