/**
 * Minimal ambient types for the virtual `cloudflare:workers` module.
 * Full Cloudflare Workers types are provided by the workerd / wrangler toolchain
 * at build time; this stub keeps the SDK package self-contained for `tsc`.
 */
declare module "cloudflare:workers" {
  export interface ExecutionContext {
    waitUntil(promise: Promise<unknown>): void;
    passThroughOnException(): void;
  }

  export class RpcTarget {}

  export class WorkerEntrypoint<Env = unknown> {
    constructor(ctx: ExecutionContext, env: Env);
    readonly ctx: ExecutionContext;
    readonly env: Env;
  }

  /** Cloudflare D1 meta object; Bookclerk {@link D1Meta} matches this subset. */
  export interface D1Meta {
    duration: number;
    changes: number;
    last_row_id: number;
    changed_db: boolean;
    rows_read: number;
    rows_written: number;
  }

  /** Cloudflare D1 query result; Bookclerk {@link D1Result} matches this shape. */
  export interface D1Result<T = Record<string, unknown>> {
    success: boolean;
    results: T[] | null;
    meta: D1Meta;
  }

  export interface D1ExecResult {
    count: number;
    duration: number;
  }

  export interface D1PreparedStatement {
    bind(...values: unknown[]): D1PreparedStatement;
    run<T = Record<string, unknown>>(): Promise<D1Result<T>>;
    all<T = Record<string, unknown>>(): Promise<D1Result<T>>;
    first<T = unknown>(colName?: string): Promise<T | null>;
    raw<T = unknown[]>(options?: { columnNames?: boolean }): Promise<T[]>;
  }

  export interface D1Database {
    prepare(query: string): D1PreparedStatement;
    batch<T = unknown>(statements: D1PreparedStatement[]): Promise<D1Result<T>[]>;
    exec(query: string): Promise<D1ExecResult>;
  }
}
