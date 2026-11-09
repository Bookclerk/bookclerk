/**
 * Contract conformance test kit: a plugin database binding that behaves
 * like the one a deployed host grants.
 *
 * A plain in-memory fake accepts anything. On a host, `env.DB` reaches an
 * isolated unit through the adapter the operator deployed: SQL is scoped to
 * the plugin's own tables, every batch is one transaction held to the
 * adapter's advertised caps, and a result past a cap is a typed
 * `limit_exceeded` error. {@link createTestDatabase} gives a Vitest or
 * `node:test` suite that same binding, backed by the `bookclerk-db-testkit`
 * binary from the Bookclerk workspace under one of three capability
 * profiles:
 *
 * - `first-party`: the shipped sqlite and Postgres adapters (256 KiB
 *   results, streamed reads).
 * - `d1`: the D1 adapter (smaller payload and statement limits).
 * - `minimum`: the smallest advertisement a host connects to (4,096-byte
 *   results, 1,024-byte cells, one row per statement, no streamed reads).
 *
 * Run plugin database code under all three ({@link TEST_DATABASE_PROFILES}):
 * what works on the shipped adapters has to work when an adapter
 * advertises only the minimums.
 *
 * ```ts
 * import { createTestDatabase, TEST_DATABASE_PROFILES } from "@bookclerk/plugin-sdk/testkit";
 *
 * for (const profile of TEST_DATABASE_PROFILES) {
 *   const db = await createTestDatabase({ profile, migrations: MIGRATIONS });
 *   try {
 *     await db.binding.prepare("INSERT INTO notes (id, body) VALUES (?, ?)").bind(1, "hi").run();
 *     const { results } = await db.binding.prepare("SELECT body FROM notes").all();
 *   } finally {
 *     await db.close();
 *   }
 * }
 * ```
 *
 * The binary is found through the `BOOKCLERK_DB_TESTKIT` environment
 * variable or `options.binary`, else as `bookclerk-db-testkit` on `PATH`
 * (`cargo build -p bookclerk-db-testkit` in the workspace builds it).
 * `bookclerk-plugin test` (`cargo plugin -- test <dir>` in the workspace)
 * runs a plugin's suite once per profile, setting both that variable and
 * `BOOKCLERK_DB_TESTKIT_PROFILE`, which is the default `profile`. This
 * module runs under Node, not inside an isolate.
 *
 * @module testkit
 */

import { spawn, type ChildProcess } from "node:child_process";
import { createInterface } from "node:readline";

import {
  createDatabaseBinding,
  decodeDbCapabilitiesReply,
  decodeExecuteResultReply,
  encodeExecuteRequest,
  type AtomicTransport,
  type DatabaseBinding,
  type ExecuteReply,
  type ExecuteRequest,
} from "./db-execute.js";
import { PluginError, parseLimitExceeded } from "./errors.js";
import type { DbCapabilities, PluginMigration } from "./generated.js";

/** The capability profiles the kit enforces, in the order conformance runs report them. */
export const TEST_DATABASE_PROFILES = ["first-party", "d1", "minimum"] as const;

/** One of {@link TEST_DATABASE_PROFILES}. */
export type TestDatabaseProfile = (typeof TEST_DATABASE_PROFILES)[number];

/**
 * Options for {@link createTestDatabase}.
 */
export interface TestDatabaseOptions {
  /**
   * Capability profile to enforce. Defaults to `BOOKCLERK_DB_TESTKIT_PROFILE`
   * when `bookclerk-plugin test` sets it, else `minimum`, the strictest.
   */
  profile?: TestDatabaseProfile;
  /**
   * The plugin's `databaseMigrations` registration, applied before the
   * binding is handed out the way the host applies it at enrollment.
   */
  migrations?: PluginMigration[];
  /** Path of the `bookclerk-db-testkit` binary; see the module docs for the default. */
  binary?: string;
}

/**
 * A served test binding and the process behind it.
 */
export interface TestDatabase {
  /** The profile being enforced. */
  readonly profile: TestDatabaseProfile;
  /** The D1-shaped binding, as `env.<BINDING>` on a host. */
  readonly binding: DatabaseBinding;
  /**
   * The profile's advertisement: the caps every batch is held to.
   *
   * @returns The capability advertisement, as `binding.limits()` reports it.
   */
  limits(): Promise<DbCapabilities>;
  /**
   * Stops the kit process; the in-memory unit goes with it.
   *
   * @returns Once the process has exited.
   */
  close(): Promise<void>;
}

/** The kit's announcement line. */
interface Serving {
  /** Loopback origin of the granted routes. */
  url: string;
  /** Bearer token of the grant. */
  token: string;
  /** Profile being enforced. */
  profile: string;
}

/**
 * Picks the kit binary: the option, then `BOOKCLERK_DB_TESTKIT`, then the
 * bare name on `PATH`.
 *
 * @param binary - Explicit path, when given.
 * @returns The command to spawn.
 */
export function testkitBinary(binary?: string): string {
  if (binary !== undefined && binary !== "") return binary;
  const env = process.env["BOOKCLERK_DB_TESTKIT"];
  if (env !== undefined && env !== "") return env;
  return "bookclerk-db-testkit";
}

/**
 * Reads the kit's single announcement line from `child`'s stdout.
 *
 * @param child - The spawned kit.
 * @returns The parsed announcement.
 * @throws Error when the process exits or closes stdout before announcing,
 *   or the line is not the expected JSON.
 */
async function readAnnouncement(child: ChildProcess): Promise<Serving> {
  const stdout = child.stdout;
  if (stdout === null) throw new Error("bookclerk-db-testkit: stdout is not piped");
  const stderr: string[] = [];
  child.stderr?.setEncoding("utf8");
  child.stderr?.on("data", (chunk: string) => stderr.push(chunk));
  // Collected until stderr closes, so a refusal written just before exit is
  // part of the error even when stdout closes first.
  const stderrClosed = new Promise<void>((resolve) => {
    if (child.stderr === null) resolve();
    else child.stderr.once("close", () => resolve());
  });
  const lines = createInterface({ input: stdout, crlfDelay: Infinity });
  const line = await new Promise<string | undefined>((resolve, reject) => {
    const done = (value: string | undefined): void => {
      lines.removeAllListeners();
      resolve(value);
    };
    lines.once("line", done);
    lines.once("close", () => done(undefined));
    child.once("error", (err) => {
      lines.removeAllListeners();
      reject(err);
    });
  });
  if (line === undefined) {
    await stderrClosed;
    throw new Error(
      `bookclerk-db-testkit exited before announcing its address${
        stderr.length > 0 ? `: ${stderr.join("").trim()}` : ""
      }`,
    );
  }
  const parsed: unknown = JSON.parse(line);
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof (parsed as Serving).url !== "string" ||
    typeof (parsed as Serving).token !== "string" ||
    typeof (parsed as Serving).profile !== "string"
  ) {
    throw new Error(`bookclerk-db-testkit announced an unexpected line: ${line}`);
  }
  return parsed as Serving;
}

/**
 * Maps a granted `/db/*` response onto the body bytes or the `PluginError`
 * the channel implies, as the isolate SDK does: `401` is `unauthorized`,
 * `413` is `limit_exceeded` with the canonical limit message parsed back
 * into `details`, `404` and `405` are `unsupported`, anything else
 * `unavailable`.
 *
 * @param resp - The route's response.
 * @param method - `GuestDatabase` method name for the error text.
 * @returns The body bytes of a 2xx response.
 */
async function databaseBody(resp: Response, method: string): Promise<Uint8Array> {
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    const message = `database grant: ${resp.status} ${text}`.trim();
    if (resp.status === 413) {
      const limit = parseLimitExceeded(text);
      throw limit === undefined
        ? PluginError.fromWire("limit_exceeded", message)
        : new PluginError("limit_exceeded", message, { ...limit });
    }
    if (resp.status === 404 || resp.status === 405) {
      throw PluginError.fromWire(
        "unsupported",
        `GuestDatabase.${method} is not served by this kit (${message})`,
      );
    }
    throw PluginError.fromWire(resp.status === 401 ? "unauthorized" : "unavailable", message);
  }
  return new Uint8Array(await resp.arrayBuffer());
}

/**
 * The granted-channel transport over the served binding.
 *
 * @param serving - The kit's announcement.
 * @returns A transport {@link createDatabaseBinding} accepts.
 */
function grantedTransport(serving: Serving): AtomicTransport {
  const headers = {
    authorization: `Bearer ${serving.token}`,
    "content-type": "application/octet-stream",
  };
  const post = async (route: string, request: ExecuteRequest, method: string): Promise<Response> =>
    fetch(`${serving.url}${route}`, {
      method: "POST",
      headers,
      body: encodeExecuteRequest(request),
    }).then(
      (resp) => resp,
      (err: unknown) => {
        throw PluginError.fromWire("unavailable", `GuestDatabase.${method}: ${String(err)}`);
      },
    );
  return {
    async execute(request: ExecuteRequest): Promise<ExecuteReply> {
      const resp = await post("/db/execute", request, "execute");
      return decodeExecuteResultReply(await databaseBody(resp, "execute"));
    },
    async executeStreamed(request: ExecuteRequest): Promise<Uint8Array> {
      const resp = await post("/db/execute-streamed", request, "executeStreamed");
      return databaseBody(resp, "executeStreamed");
    },
    async limits(): Promise<DbCapabilities> {
      const resp = await fetch(`${serving.url}/db/limits`, {
        headers: { authorization: headers.authorization },
      });
      return decodeDbCapabilitiesReply(await databaseBody(resp, "limits"));
    },
  };
}

/** One migration operation as the kit's request line spells it. */
type KitMigrationOp = { schema: string } | { data: string };

/**
 * Spells a `databaseMigrations` registration the way the kit reads it.
 *
 * A plugin may return each operation typed (`{ kind: "schema", value }`,
 * the {@link PluginMigration} shape) or in the short form the
 * `schemaMigrationOp` / `dataMigrationOp` helpers build (`{ schema }`); the
 * host accepts both, so the kit is handed the one form it parses.
 *
 * @param migrations - The registration, in either form.
 * @returns The same migrations with every operation as `{ schema }` or `{ data }`.
 * @throws Error naming the migration when an operation is neither form.
 */
export function kitMigrations(
  migrations: readonly PluginMigration[],
): { id: string; operations: KitMigrationOp[] }[] {
  return migrations.map((migration) => ({
    id: migration.id,
    operations: migration.operations.map((op): KitMigrationOp => {
      const o = op as { kind?: unknown; value?: unknown; schema?: unknown; data?: unknown };
      if (typeof o.schema === "string") return { schema: o.schema };
      if (typeof o.data === "string") return { data: o.data };
      if (o.kind === "schema" && typeof o.value === "string") return { schema: o.value };
      if (o.kind === "data" && typeof o.value === "string") return { data: o.value };
      throw new Error(
        `migration ${JSON.stringify(migration.id)} has an operation without schema or data`,
      );
    }),
  }));
}

/**
 * The profile a test runs under: the option, else the one
 * `bookclerk-plugin test` names in `BOOKCLERK_DB_TESTKIT_PROFILE`, else
 * `minimum`.
 *
 * @param option - The caller's `profile` option.
 * @returns The profile name, unchecked.
 */
export function defaultProfile(option: TestDatabaseProfile | undefined): TestDatabaseProfile {
  if (option !== undefined) return option;
  const fromEnv = process.env["BOOKCLERK_DB_TESTKIT_PROFILE"];
  return fromEnv === undefined || fromEnv === "" ? "minimum" : (fromEnv as TestDatabaseProfile);
}

/**
 * Spawns the kit under `options.profile` with `options.migrations` applied
 * and returns the served binding.
 *
 * @param options - Profile, registration and binary location.
 * @returns The binding and its process; call `close()` when the test ends.
 * @throws Error when the binary cannot be started, refuses the profile or
 *   the registration (the kit's message is included), or exits early.
 */
export async function createTestDatabase(options: TestDatabaseOptions = {}): Promise<TestDatabase> {
  const profile = defaultProfile(options.profile);
  if (!TEST_DATABASE_PROFILES.includes(profile)) {
    throw new Error(
      `unknown test database profile ${JSON.stringify(profile)} (one of ${TEST_DATABASE_PROFILES.join(", ")})`,
    );
  }
  const child = spawn(testkitBinary(options.binary), ["serve"], {
    stdio: ["pipe", "pipe", "pipe"],
  });
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
    child.once("error", () => resolve());
  });
  const announced = readAnnouncement(child);
  child.stdin?.write(
    `${JSON.stringify({ profile, migrations: kitMigrations(options.migrations ?? []) })}\n`,
  );
  let serving: Serving;
  try {
    serving = await announced;
  } catch (err) {
    child.stdin?.end();
    child.kill();
    await exited;
    throw err;
  }
  const transport = grantedTransport(serving);
  const binding = createDatabaseBinding(transport);
  let closed: Promise<void> | undefined;
  return {
    profile,
    binding,
    limits: () => binding.limits(),
    close(): Promise<void> {
      if (closed === undefined) {
        // The kit serves until its stdin closes; a process that lingers is
        // killed so a hung test run does not leak it.
        child.stdin?.end();
        const timer = setTimeout(() => child.kill(), 5_000);
        closed = exited.then(() => clearTimeout(timer));
      }
      return closed;
    },
  };
}
