// A small D1 stand-in over node:sqlite for the demo tests, with the
// migrations in ../migrations applied. Only the calls the demo makes are
// implemented: prepare().bind().all() / first() / run() and batch(); exec()
// comes along so the class is a whole D1Database.

import { readdirSync, readFileSync } from "node:fs";
import { DatabaseSync, type SQLInputValue } from "node:sqlite";
import { URL } from "node:url";

/**
 * A bound value as node:sqlite takes it; undefined binds as NULL, as D1
 * would refuse it but the demo never passes one on purpose.
 *
 * @param value - Value passed to `bind`.
 * @returns The value for node:sqlite.
 * @throws {TypeError} For a value SQLite cannot store.
 */
function sqlValue(value: unknown): SQLInputValue {
  if (value === undefined || value === null) return null;
  if (typeof value === "number" || typeof value === "bigint" || typeof value === "string") return value;
  if (value instanceof Uint8Array) return value;
  throw new TypeError(`cannot bind a ${typeof value}`);
}

/**
 * D1's result metadata for a statement that changed `changes` rows.
 *
 * @param changes - Rows changed.
 * @returns The metadata.
 */
function meta(changes: number): D1Meta & Record<string, unknown> {
  return { duration: 0, size_after: 0, rows_read: 0, rows_written: changes, last_row_id: 0, changed_db: changes > 0, changes };
}

/** A prepared statement with bound values. */
class Statement implements D1PreparedStatement {
  /** Database. */
  readonly db: DatabaseSync;
  /** Statement text. */
  readonly sql: string;
  /** Bound values. */
  readonly params: SQLInputValue[];

  /**
   * @param db - Database.
   * @param sql - Statement text.
   * @param params - Bound values.
   */
  constructor(db: DatabaseSync, sql: string, params: SQLInputValue[] = []) {
    this.db = db;
    this.sql = sql;
    this.params = params;
  }

  /**
   * @param params - Values for the placeholders.
   * @returns A copy with these values bound.
   */
  bind(...params: unknown[]): Statement {
    return new Statement(this.db, this.sql, params.map(sqlValue));
  }

  /**
   * Rows as plain objects. Like D1's own, the row type is the caller's word.
   *
   * @returns Rows.
   */
  rows<T>(): T[] {
    return this.db.prepare(this.sql).all(...this.params).map((r) => ({ ...r }) as T);
  }

  /** @returns Rows. */
  async all<T = Record<string, unknown>>(): Promise<D1Result<T>> {
    return { success: true, meta: meta(0), results: this.rows<T>() };
  }

  first<T = unknown>(colName: string): Promise<T | null>;
  first<T = Record<string, unknown>>(): Promise<T | null>;
  /**
   * @param colName - One column, or the whole row when left out.
   * @returns First row (or its column), or null.
   */
  async first<T>(colName?: string): Promise<T | null> {
    const row = this.db.prepare(this.sql).get(...this.params);
    if (!row) return null;
    return (colName === undefined ? { ...row } : row[colName]) as T;
  }

  /** @returns Rows changed. */
  async run<T = Record<string, unknown>>(): Promise<D1Result<T>> {
    const res = this.db.prepare(this.sql).run(...this.params);
    return { success: true, meta: meta(Number(res.changes)), results: [] };
  }

  raw<T = unknown[]>(options: { columnNames: true }): Promise<[string[], ...T[]]>;
  raw<T = unknown[]>(options?: { columnNames?: false }): Promise<T[]>;
  /**
   * Not used by the demo.
   *
   * @throws {Error} Always.
   */
  raw(): Promise<never> {
    throw new Error("raw() is not implemented by the test D1");
  }
}

/** A fresh in-memory D1 stand-in. */
export class TestD1 implements D1Database {
  /** The node:sqlite database, for tests that look at it directly. */
  readonly raw: DatabaseSync;

  /**
   * @param raw - Database with the schema applied.
   */
  constructor(raw: DatabaseSync) {
    this.raw = raw;
  }

  /**
   * @param sql - Statement text.
   * @returns The statement.
   */
  prepare(sql: string): Statement {
    return new Statement(this.raw, sql);
  }

  /**
   * Runs statements in one transaction.
   *
   * @param stmts - Statements from `prepare`.
   * @returns One result per statement.
   */
  async batch<T = unknown>(stmts: D1PreparedStatement[]): Promise<D1Result<T>[]> {
    this.raw.exec("BEGIN");
    try {
      const out: D1Result<T>[] = [];
      for (const s of stmts) out.push(await s.run<T>());
      this.raw.exec("COMMIT");
      return out;
    } catch (err) {
      this.raw.exec("ROLLBACK");
      throw err;
    }
  }

  /**
   * @param sql - SQL statements.
   * @returns A count of zero.
   */
  async exec(sql: string): Promise<D1ExecResult> {
    this.raw.exec(sql);
    return { count: 0, duration: 0 };
  }

  /**
   * Not used by the demo.
   *
   * @throws {Error} Always.
   */
  withSession(): D1DatabaseSession {
    throw new Error("sessions are not implemented by the test D1");
  }

  /**
   * Not used by the demo.
   *
   * @throws {Error} Always.
   */
  dump(): Promise<ArrayBuffer> {
    throw new Error("dump() is not implemented by the test D1");
  }
}

/**
 * A fresh in-memory database with the demo schema.
 *
 * @returns D1-shaped handle.
 */
export function createD1(): TestD1 {
  const db = new DatabaseSync(":memory:");
  const dir = new URL("../migrations/", import.meta.url);
  for (const name of readdirSync(dir).filter((n) => n.endsWith(".sql")).sort()) {
    db.exec(readFileSync(new URL(name, dir), "utf8"));
  }
  return new TestD1(db);
}
