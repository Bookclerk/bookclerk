// The demo tests' D1 databases: real D1 that Miniflare runs inside the
// Workers runtime. vitest.config.ts binds TEST_DB_POOL_SIZE of them and
// workers-setup.ts empties them all before each test. Each createD1() call
// hands out a database no one has used in the current test, and applies
// ../migrations to it before its first statement runs, so a test that never
// touches its database pays nothing for it.

import { applyD1Migrations, env } from "cloudflare:test";

import { TEST_DB_POOL_SIZE } from "./d1-pool.ts";

/** Databases handed out to the current test. */
let used = 0;

/** Called by workers-setup.ts once the databases are empty again. */
export function resetHandOut(): void {
  used = 0;
}

/**
 * A pool database by index.
 *
 * @param i - Index below TEST_DB_POOL_SIZE.
 * @returns The binding.
 * @throws {Error} When vitest.config.ts did not bind it.
 */
function pooled(i: number): D1Database {
  const db = (env as unknown as Record<string, D1Database | undefined>)[`TEST_DB_${i}`];
  if (db === undefined) throw new Error(`TEST_DB_${i} is not bound`);
  return db;
}

/**
 * A statement that waits for the schema, then runs on the real database.
 * batch() reads `sql` and `params` back to rebuild it there.
 */
class LazyStatement {
  /** The database, once migrated. */
  readonly db: Promise<D1Database>;
  /** Statement text. */
  readonly sql: string;
  /** Bound values, in order. */
  readonly params: unknown[];

  /**
   * @param db - The database, once migrated.
   * @param sql - Statement text.
   * @param params - Bound values.
   */
  constructor(db: Promise<D1Database>, sql: string, params: unknown[] = []) {
    this.db = db;
    this.sql = sql;
    this.params = params;
  }

  /**
   * @param values - Values for the placeholders.
   * @returns A statement with them bound.
   */
  bind(...values: unknown[]): LazyStatement {
    return new LazyStatement(this.db, this.sql, values);
  }

  /** @returns The statement on the migrated database. */
  async real(): Promise<D1PreparedStatement> {
    return (await this.db).prepare(this.sql).bind(...this.params);
  }

  /**
   * @param column - Optional column to read.
   * @returns The first row, or that column of it.
   */
  async first(column?: string): Promise<unknown> {
    const stmt = await this.real();
    return column === undefined ? stmt.first() : stmt.first(column);
  }

  /** @returns The result with every row. */
  async all(): Promise<D1Result> {
    return (await this.real()).all();
  }

  /** @returns The result without rows. */
  async run(): Promise<D1Result> {
    return (await this.real()).run();
  }

  /** @returns The rows as arrays. */
  async raw(): Promise<unknown[]> {
    return (await this.real()).raw();
  }
}

/**
 * A database with the demo schema and no rows, not shared with anything
 * else in the current test.
 *
 * @returns A D1Database that migrates on first use.
 * @throws {Error} When the test has used every database in the pool.
 */
export function createD1(): D1Database {
  if (used >= TEST_DB_POOL_SIZE)
    throw new Error(`a test may create at most ${TEST_DB_POOL_SIZE} databases`);
  const db = pooled(used);
  used += 1;
  let ready: Promise<D1Database> | undefined;
  const migrated = (): Promise<D1Database> =>
    (ready ??= applyD1Migrations(db, env.TEST_MIGRATIONS).then(() => db));
  const lazy = {
    prepare: (sql: string) => new LazyStatement(Promise.resolve().then(migrated), sql),
    batch: async (stmts: LazyStatement[]) =>
      (await migrated()).batch(await Promise.all(stmts.map((s) => s.real()))),
    exec: async (sql: string) => (await migrated()).exec(sql),
  };
  return lazy as unknown as D1Database;
}
