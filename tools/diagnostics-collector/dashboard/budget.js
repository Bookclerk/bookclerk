/**
 * Per-invocation D1 call budget. On Workers Free, D1 allows 50 queries per
 * Worker invocation; going past it fails the call that crosses the line. The
 * dashboard wraps its database handle once per invocation so every call is
 * counted, and long-running work (the cron sync, the owner's reindex) checks
 * what is left before each unit of work and saves its cursor in the same
 * batch as that work, so a later invocation resumes where this one stopped.
 *
 * A `batch()` counts as one call, as D1 runs it as one request.
 */

/** D1 queries one invocation may make on Workers Free. */
export const D1_QUERIES_PER_INVOCATION = 50;

/** Thrown when a call would go past the invocation's budget. */
export class BudgetExceeded extends Error {
  /**
   * @param {number} limit The budget that would have been exceeded.
   */
  constructor(limit) {
    super(`D1 call budget of ${limit} per invocation exhausted`);
    /** Error name, for logs. */
    this.name = "BudgetExceeded";
  }
}

/** Calls an invocation has made against a fixed limit. */
export class Budget {
  /**
   * @param {number} limit Most calls allowed; a positive safe integer.
   */
  constructor(limit) {
    /** Most calls allowed in this invocation. */
    this.limit = limit;
    /** Calls made so far. */
    this.used = 0;
  }

  /** @returns {number} Calls still allowed. */
  left() {
    return this.limit - this.used;
  }

  /**
   * Records one call.
   *
   * @returns {void}
   * @throws {BudgetExceeded} When the budget is already spent; the call is
   *   then not made.
   */
  take() {
    if (this.used >= this.limit) throw new BudgetExceeded(this.limit);
    this.used += 1;
  }
}

/**
 * A prepared statement that charges the budget when it runs.
 *
 * @param {D1PreparedStatement} inner The real statement.
 * @param {Budget} budget Budget charged.
 * @returns {object} D1-shaped statement; `inner` is what `batch()` runs.
 */
function meteredStatement(inner, budget) {
  return {
    inner,
    bind: (...params) => meteredStatement(inner.bind(...params), budget),
    all: () => (budget.take(), inner.all()),
    first: (col) => (budget.take(), inner.first(col)),
    run: () => (budget.take(), inner.run()),
    raw: (opts) => (budget.take(), inner.raw(opts)),
  };
}

/**
 * Wraps a D1 handle so every call charges `budget`. Wrapping an already
 * wrapped handle returns it unchanged, so one invocation shares one budget.
 *
 * @param {D1Database} db Database.
 * @param {Budget} [budget] Budget to charge; a fresh Workers Free budget when
 *   left out.
 * @returns {D1Database & {budget: Budget}} The metered handle.
 */
export function metered(db, budget = new Budget(D1_QUERIES_PER_INVOCATION)) {
  if (db && db.budget instanceof Budget) return db;
  return {
    budget,
    prepare: (sql) => meteredStatement(db.prepare(sql), budget),
    batch: (stmts) => {
      budget.take();
      return db.batch(stmts.map((s) => s.inner ?? s));
    },
    exec: (sql) => {
      budget.take();
      return db.exec(sql);
    },
  };
}
