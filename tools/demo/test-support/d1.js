// A small D1 stand-in over node:sqlite for the demo tests, with the
// migrations in ../migrations applied. Only the calls the demo makes are
// implemented: prepare().bind().all() / first() / run() and batch().

import { readdirSync, readFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";

/** A prepared statement with bound values. */
class Statement {
  /**
   * @param {DatabaseSync} db Database.
   * @param {string} sql Statement text.
   * @param {unknown[]} params Bound values.
   */
  constructor(db, sql, params = []) {
    /** Database. */
    this.db = db;
    /** Statement text. */
    this.sql = sql;
    /** Bound values. */
    this.params = params;
  }

  /** @returns {Statement} A copy with these values bound. */
  bind(...params) {
    return new Statement(this.db, this.sql, params.map((p) => (p === undefined ? null : p)));
  }

  /** @returns {Promise<{results: object[]}>} Rows. */
  async all() {
    return { results: this.db.prepare(this.sql).all(...this.params).map((r) => ({ ...r })) };
  }

  /** @returns {Promise<object | null>} First row. */
  async first() {
    const row = this.db.prepare(this.sql).get(...this.params);
    return row ? { ...row } : null;
  }

  /** @returns {Promise<{meta: {changes: number}}>} Rows changed. */
  async run() {
    const res = this.db.prepare(this.sql).run(...this.params);
    return { meta: { changes: Number(res.changes) } };
  }
}

/**
 * A fresh in-memory database with the demo schema.
 *
 * @returns {{prepare(sql: string): Statement, batch(s: Statement[]): Promise<unknown[]>, raw: DatabaseSync}}
 *   D1-shaped handle.
 */
export function createD1() {
  const db = new DatabaseSync(":memory:");
  const dir = new URL("../migrations/", import.meta.url);
  for (const name of readdirSync(dir).filter((n) => n.endsWith(".sql")).sort()) {
    db.exec(readFileSync(new URL(name, dir), "utf8"));
  }
  return {
    raw: db,
    prepare: (sql) => new Statement(db, sql),
    async batch(stmts) {
      db.exec("BEGIN");
      try {
        const out = [];
        for (const s of stmts) out.push(await s.run());
        db.exec("COMMIT");
        return out;
      } catch (err) {
        db.exec("ROLLBACK");
        throw err;
      }
    },
  };
}
