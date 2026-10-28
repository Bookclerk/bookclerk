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
   * @param limit - The budget that would have been exceeded.
   */
  constructor(limit: number) {
    super(`D1 call budget of ${limit} per invocation exhausted`);
    this.name = "BudgetExceeded";
  }
}

/** Calls an invocation has made against a fixed limit. */
export class Budget {
  /** Most calls allowed in this invocation. */
  readonly limit: number;
  /** Calls made so far. */
  used: number = 0;

  /**
   * @param limit - Most calls allowed; a positive safe integer.
   */
  constructor(limit: number) {
    this.limit = limit;
  }

  /** @returns Calls still allowed. */
  left(): number {
    return this.limit - this.used;
  }

  /**
   * Records one call.
   *
   * @throws {BudgetExceeded} When the budget is already spent; the call is
   *   then not made.
   */
  take(): void {
    if (this.used >= this.limit) throw new BudgetExceeded(this.limit);
    this.used += 1;
  }
}

/** A prepared statement that charges the budget when it runs. */
class MeteredStatement implements D1PreparedStatement {
  /** The real statement; what `batch()` runs. */
  readonly inner: D1PreparedStatement;
  /** Budget charged. */
  readonly budget: Budget;

  /**
   * @param inner - The real statement.
   * @param budget - Budget charged.
   */
  constructor(inner: D1PreparedStatement, budget: Budget) {
    this.inner = inner;
    this.budget = budget;
  }

  /**
   * Binds values (free: nothing runs yet).
   *
   * @param values - Values for the `?` placeholders.
   * @returns The bound statement, still metered.
   */
  bind(...values: unknown[]): MeteredStatement {
    return new MeteredStatement(this.inner.bind(...values), this.budget);
  }

  first<T = unknown>(colName: string): Promise<T | null>;
  first<T = Record<string, unknown>>(): Promise<T | null>;
  /**
   * Runs the statement for its first row, charging one call.
   *
   * @param colName - One column of the row, or the whole row when left out.
   * @returns The row or column value, or null without a row.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  first<T>(colName?: string): Promise<T | null> {
    this.budget.take();
    return colName === undefined ? this.inner.first<T>() : this.inner.first<T>(colName);
  }

  /**
   * Runs the statement for its effect, charging one call.
   *
   * @returns D1's result.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  run<T = Record<string, unknown>>(): Promise<D1Result<T>> {
    this.budget.take();
    return this.inner.run<T>();
  }

  /**
   * Runs the statement for all rows, charging one call.
   *
   * @returns D1's result.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  all<T = Record<string, unknown>>(): Promise<D1Result<T>> {
    this.budget.take();
    return this.inner.all<T>();
  }

  raw<T = unknown[]>(options: { columnNames: true }): Promise<[string[], ...T[]]>;
  raw<T = unknown[]>(options?: { columnNames?: false }): Promise<T[]>;
  /**
   * Runs the statement for rows as arrays, charging one call.
   *
   * @param options - `columnNames: true` puts the column names first.
   * @returns The rows.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  raw<T = unknown[]>(options?: { columnNames?: boolean }): Promise<[string[], ...T[]] | T[]> {
    this.budget.take();
    return options?.columnNames ? this.inner.raw<T>({ columnNames: true }) : this.inner.raw<T>();
  }
}

/**
 * A D1 handle whose every call charges `budget`. Sessions and `dump()` are
 * not offered: nothing in the dashboard uses them, and a session's calls
 * would go uncounted.
 */
export class MeteredD1 implements D1Database {
  /** The real database. */
  readonly inner: D1Database;
  /** Budget charged. */
  readonly budget: Budget;

  /**
   * @param inner - The real database.
   * @param budget - Budget charged.
   */
  constructor(inner: D1Database, budget: Budget) {
    this.inner = inner;
    this.budget = budget;
  }

  /**
   * Prepares a statement (free: nothing runs yet).
   *
   * @param query - SQL with `?` placeholders.
   * @returns The metered statement.
   */
  prepare(query: string): MeteredStatement {
    return new MeteredStatement(this.inner.prepare(query), this.budget);
  }

  /**
   * Runs statements in one request, charging one call.
   *
   * @param statements - Statements from `prepare`.
   * @returns One result per statement.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  batch<T = unknown>(statements: D1PreparedStatement[]): Promise<D1Result<T>[]> {
    this.budget.take();
    return this.inner.batch<T>(
      statements.map((s) => (s instanceof MeteredStatement ? s.inner : s)),
    );
  }

  /**
   * Runs raw SQL, charging one call.
   *
   * @param query - SQL statements.
   * @returns D1's result.
   * @throws {BudgetExceeded} When the budget is spent.
   */
  exec(query: string): Promise<D1ExecResult> {
    this.budget.take();
    return this.inner.exec(query);
  }

  /**
   * Not offered (see the class comment).
   *
   * @throws {Error} Always.
   */
  withSession(): D1DatabaseSession {
    throw new Error("a metered D1 handle has no sessions");
  }

  /**
   * Not offered (see the class comment).
   *
   * @throws {Error} Always.
   */
  dump(): Promise<ArrayBuffer> {
    throw new Error("a metered D1 handle has no dump");
  }
}

/**
 * Wraps a D1 handle so every call charges `budget`. Wrapping an already
 * wrapped handle returns it unchanged, so one invocation shares one budget.
 *
 * @param db - Database.
 * @param budget - Budget to charge; a fresh Workers Free budget when
 *   left out.
 * @returns The metered handle.
 */
export function metered(
  db: D1Database,
  budget: Budget = new Budget(D1_QUERIES_PER_INVOCATION),
): MeteredD1 {
  if (db instanceof MeteredD1) return db;
  return new MeteredD1(db, budget);
}
