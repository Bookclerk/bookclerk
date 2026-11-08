/**
 * Typed Cap'n `ExecuteRequest` codec and host-mediated `DatabaseBinding`.
 *
 * `encodeExecuteRequest` emits the same unpacked Cap'n stream as the Rust SDK.
 * `DatabaseBinding.execute` builds that message (for `maxRequestBytes`) and
 * forwards the structured request through a host `execute` transport.
 */

import {
  DB_TYPES,
  DB_RESULT_SELECTIONS,
  DB_STATEMENT_KINDS,
  type DbResultSelection,
  type DbStatementKind,
} from "./abi.js";
import { CapnpMessage, CapnpReader, type CapnpStruct, type StructReader } from "./db-capnp.js";
import { PluginError } from "./errors.js";
import type { DbCapabilities, ExecuteStreamFrame } from "./generated.js";
import { DbCapabilitiesReplyCodec, decodeMessage, encodeMessage } from "./generated-wire.js";
import { readDbValue, writeDbValue, type DbType, type DbValue } from "./db-value.js";
import {
  ExecuteStreamDecoder,
  executeStreamLimits,
  streamChunks,
  streamsResults,
} from "./db-stream.js";
import { guestStatementKind, splitExecQueries } from "./guest-sql.js";

// Ordinal tables come from the generated `abi.ts` projection of
// `schema/plugin.capnp` (index = Cap'n Proto ordinal).
export type { DbResultSelection, DbStatementKind } from "./abi.js";

const KIND_FROM = DB_STATEMENT_KINDS;
const KIND_ORD: Record<DbStatementKind, number> = Object.fromEntries(
  DB_STATEMENT_KINDS.map((kind, ord) => [kind, ord]),
) as Record<DbStatementKind, number>;

const SELECT_FROM = DB_RESULT_SELECTIONS;
const SELECT_ORD: Record<DbResultSelection, number> = Object.fromEntries(
  DB_RESULT_SELECTIONS.map((sel, ord) => [sel, ord]),
) as Record<DbResultSelection, number>;

const COL_TYPE_FROM = DB_TYPES;
const COL_TYPE_ORD: Record<DbType, number> = Object.fromEntries(
  DB_TYPES.map((ty, ord) => [ty, ord]),
) as Record<DbType, number>;

/**
 * One statement in a typed atomic batch.
 */
export interface TypedDbStatement {
  /** Canonical Bookclerk SQL (`?` placeholders). */
  sql: string;
  /** Ordered typed binds. */
  parameters: DbValue[];
  /** Host-authored kind (adapters must not reparse SQL). */
  kind: DbStatementKind;
  /** Proven row upper bound (`0` = unproven). */
  maxRows: number;
  /** Which result fields the caller needs. */
  resultSelection: DbResultSelection;
}

/**
 * Typed `execute` request.
 */
export interface ExecuteRequest {
  /** Caller-chosen idempotency key. */
  operationId: string;
  /** SHA-256 hex of the idempotency-relevant request; empty when omitted. */
  requestHash: string;
  /** Ordered statements (must be non-empty). */
  statements: TypedDbStatement[];
  /** Guest-visible deadline (unix ms). Zero means omitted. */
  deadlineUnixMs: number;
}

/**
 * One column in a typed result set.
 */
export interface DbColumn {
  /** Column name. */
  name: string;
  /** Declared / inferred type. */
  dbType: DbType;
}

/**
 * One positional result row.
 */
export interface DbRow {
  /** Cells in column order. */
  values: DbValue[];
}

/**
 * Result of one statement in an atomic batch.
 */
export interface StatementResult {
  /** Positional rows. */
  rows: DbRow[];
  /** Column metadata aligned with each row. */
  columns: DbColumn[];
  /** Engine `rowsAffected` (0 for `Select`). */
  rowsAffected: number;
}

/**
 * Handler/engine timing on `ExecuteReply`.
 */
export interface DbTiming {
  /** Whole-attempt elapsed microseconds. */
  attemptElapsedUs: number;
  /** Engine-reported SQL microseconds when available. */
  dbExecutionUs: number;
  /** Observability source (`sqlite_txn`, `d1_sql_duration`, …). */
  dbTimingSource: string;
}

/**
 * Typed `execute` reply.
 */
export interface ExecuteReply {
  /** Echo of the request `operationId`. */
  operationId: string;
  /** Per-statement results, in plan order. */
  statements: StatementResult[];
  /** Handler/engine timing. */
  timing: DbTiming;
}

/**
 * Host `GuestDatabase.execute` projection.
 */
export interface AtomicTransport {
  /**
   * Runs one typed atomic batch.
   *
   * @param request Structured Cap'n `ExecuteRequest`.
   * @returns Structured `ExecuteReply`.
   */
  execute(request: ExecuteRequest): Promise<ExecuteReply>;
  /**
   * Negotiated limits of the session (`DbCapabilities`). Transports that
   * predate `GuestDatabase.limits` leave it out; the binding then throws
   * `unsupported`.
   *
   * @returns The adapter's capability advertisement for this session.
   */
  limits?(): Promise<DbCapabilities>;
  /**
   * Runs the same typed atomic batch as {@link AtomicTransport.execute} but
   * answered as `ExecuteStreamFrame` bytes (`GuestDatabase.executeStreamed`),
   * so the result may grow to the session's streamed budget. Transports
   * that predate it leave it out; the binding then reads inline.
   *
   * @param request Structured Cap'n `ExecuteRequest`.
   * @returns The frame sequence, whole or as a byte stream.
   */
  executeStreamed?(request: ExecuteRequest): Promise<ReadableStream<Uint8Array> | Uint8Array>;
}

/**
 * Explicit retry identity. Reuses both `operationId` and `requestHash`.
 */
export interface RetryToken {
  /** Caller-chosen idempotency key. */
  operationId: string;
  /** Canonical Cap'n request hash stamped by the host. */
  requestHash: string;
}

/**
 * Cloudflare {@link https://developers.cloudflare.com/d1/worker-api/return-object/ | D1Result.meta}
 * shape (Bookclerk fills fields available from the typed execute reply).
 */
export interface D1Meta {
  /** Engine-reported SQL duration in milliseconds. */
  duration: number;
  /** Rows changed by the statement (`rowsAffected`). */
  changes: number;
  /** Last inserted row id when the adapter exposes it (else `0`). */
  last_row_id: number;
  /** `true` when `changes > 0`. */
  changed_db: boolean;
  /** Rows returned to the guest for this statement. */
  rows_read: number;
  /** Rows written (`changes` for DML). */
  rows_written: number;
}

/**
 * Cloudflare {@link https://developers.cloudflare.com/d1/worker-api/return-object/ | D1Result}
 * projection for plugin guests. Errors throw; successful calls always set `success: true`.
 */
export interface D1Result<T = Record<string, DbValue>> {
  success: true;
  /** Row objects for selects; `[]` when empty; `null` when not applicable (DML). */
  results: T[] | null;
  meta: D1Meta;
}

/**
 * Cloudflare {@link https://developers.cloudflare.com/d1/worker-api/return-object/ | D1ExecResult}.
 */
export interface D1ExecResult {
  /** Number of statements executed (one per non-empty `exec` line). */
  count: number;
  /** Total duration in milliseconds. */
  duration: number;
}

/**
 * Options for {@link createDatabaseBinding}.
 */
export interface DatabaseBindingOptions {
  /** Negotiated `maxRequestBytes` (`0` = unlimited). */
  maxRequestBytes?: number;
  /** Default `maxRows` for `all()` (`0` = host adapter cap). */
  maxResultRows?: number;
  /** Default retry token; omitted calls mint a UUID and leave the hash empty. */
  retry?: RetryToken;
  /** Idempotency key; generated when omitted. */
  operationId?: string;
  /** SHA-256 hex of the request; empty when omitted. */
  requestHash?: string;
  /** Guest-visible deadline (unix ms). */
  deadlineUnixMs?: number;
}

/**
 * Cloudflare-style prepared statement. Kind, bounds, and request hash are
 * derived by the trusted host. Only universal {@link DbValue}s are public.
 */
export interface PreparedStatement {
  /**
   * Replace bound parameters.
   *
   * @param values Universal `DbValue`s only.
   */
  bind(...values: DbValue[]): PreparedStatement;
  /**
   * Execute as DML. Returns a Cloudflare-shaped {@link D1Result}.
   *
   * @param options Optional retry token.
   */
  run(options?: { retry?: RetryToken }): Promise<D1Result>;
  /**
   * First row as a name→value map, or one column when `colName` is set.
   *
   * @param colName Optional column name (Cloudflare `first(colName)`).
   * @param options Optional retry token.
   */
  first(
    colName?: string,
    options?: { retry?: RetryToken },
  ): Promise<Record<string, DbValue> | DbValue | null>;
  /**
   * Positional cell values per row (Cloudflare `raw()`).
   *
   * @param options Optional retry token.
   */
  raw(options?: { retry?: RetryToken }): Promise<DbValue[][]>;
  /**
   * Execute as a row-returning query. Returns a Cloudflare-shaped {@link D1Result}.
   *
   * @param options Optional retry token.
   */
  all(options?: { retry?: RetryToken }): Promise<D1Result>;
  /**
   * Rows one at a time from the same single-snapshot streamed read as
   * {@link PreparedStatement.all}, without holding the whole result:
   * `for await (const row of stmt.iterate()) …`. The read still fails closed
   * (`limit_exceeded`) past the session's streamed budget. On a session that
   * does not stream the rows come from one inline `all()`.
   *
   * @param options Optional retry token.
   */
  iterate(options?: { retry?: RetryToken }): AsyncIterable<Record<string, DbValue>>;
  /** Mark DML intent for {@link DatabaseBinding.batch}. */
  asRun(): PreparedStatement;
  /** Mark `maxRows = 1` row intent for {@link DatabaseBinding.batch}. */
  asFirst(): PreparedStatement;
  /** Mark row-returning intent for {@link DatabaseBinding.batch}. */
  asAll(): PreparedStatement;
}

/**
 * Host-mediated typed SQL surface for plugin guests.
 *
 * Public API is Cloudflare-style `prepare().bind().run()/first()/all()` plus
 * atomic `batch()`. Raw `execute` stays internal.
 */
export interface DatabaseBinding {
  /**
   * Prepare one canonical-SQL statement (`?` placeholders).
   *
   * @param sql Host-mediated SQL. Kind and bounds are derived by the host.
   */
  prepare(sql: string): PreparedStatement;
  /**
   * Run prepared statements as one typed atomic batch.
   *
   * Ordinary bound prepared statements use Cloudflare `run()` semantics
   * (`resultSelection: rows`) by default. Optional `asRun` / `asFirst` /
   * `asAll` override per-statement intent. Returns one Cloudflare-shaped
   * {@link D1Result} per statement.
   *
   * @param statements Prepared statements (binds and intent already applied).
   * @param options Retry token.
   */
  batch(statements: PreparedStatement[], options?: { retry?: RetryToken }): Promise<D1Result[]>;
  /**
   * Execute raw SQL without bind parameters (Cloudflare `D1Database.exec`).
   *
   * @param query Canonical SQL string.
   * @param options Optional retry token.
   */
  exec(query: string, options?: { retry?: RetryToken }): Promise<D1ExecResult>;
  /**
   * Internal typed-batch transport. Prefer {@link DatabaseBinding.prepare}.
   *
   * @param batch Ordered statements with typed `DbValue` parameters.
   * @param options Optional retry token.
   */
  execute(batch: TypedDbStatement[], options?: { retry?: RetryToken }): Promise<ExecuteReply>;
  /**
   * Splits `statements` into batches that each fit one request
   * (`maxStatements` of the session and the tighter of the session's and
   * the binding's `maxRequestBytes`, or the caps in `options`):
   * `for await (const b of db.chunk(stmts)) await db.batch(b)`. Each batch
   * is atomic; the loop is not. A single statement larger than the request
   * cap is yielded alone, so `batch()` reports the typed error. Batches are
   * sized with the UUID-shaped retry id the binding mints; a caller-chosen
   * retry id that is longer leaves that much less room.
   *
   * @param statements Prepared statements (binds and intent already applied).
   * @param options Caps to split by instead of the session's limits; `0`
   *   given explicitly means no cap (one batch when both are `0`).
   */
  chunk(
    statements: PreparedStatement[],
    options?: { maxStatements?: number; maxRequestBytes?: number },
  ): AsyncIterable<PreparedStatement[]>;
  /**
   * The negotiated limits of this binding's session (`DbCapabilities`):
   * `maxRequestBytes`, `maxStatements`, `maxResultBytes`, `maxResultRows`
   * and the rest. Use them to size a batch or a read before sending.
   *
   * @throws `PluginError` with code `unsupported` on a host that predates
   * `GuestDatabase.limits`.
   */
  limits(): Promise<DbCapabilities>;
}

/**
 * Encodes a standalone unpacked Cap'n `ExecuteRequest` message.
 *
 * @param request Structured request (non-empty `statements`).
 * @returns Unpacked Cap'n stream bytes (same encoding as the Rust SDK).
 * @throws When `statements` is empty.
 */
export function encodeExecuteRequest(request: ExecuteRequest): Uint8Array {
  if (request.statements.length === 0) {
    throw new Error("execute statements must be non-empty");
  }
  const msg = new CapnpMessage();
  const root = msg.initRoot(4, 3);
  root.setText(0, request.operationId);
  root.setText(1, request.requestHash);
  root.setUint64(3, BigInt(request.deadlineUnixMs));
  const stmts = root.initStructList(2, request.statements.length, 1, 2);
  for (let i = 0; i < request.statements.length; i++) {
    writeStatement(stmts[i], request.statements[i]);
  }
  return msg.finish();
}

/**
 * Decodes a standalone unpacked Cap'n `ExecuteRequest` message.
 *
 * @param bytes Unpacked Cap'n stream.
 * @returns Structured request.
 * @throws When the buffer is not a valid non-empty `ExecuteRequest`.
 */
export function decodeExecuteRequest(bytes: Uint8Array): ExecuteRequest {
  const reader = new CapnpReader(bytes);
  const root = reader.root(4, 3);
  const stmtStructs = root.getStructList(2, 1, 2);
  if (stmtStructs.length === 0) {
    throw new Error("execute statements must be non-empty");
  }
  return {
    operationId: root.getText(0),
    requestHash: root.getText(1),
    statements: stmtStructs.map(readStatement),
    deadlineUnixMs: Number(root.getUint64(3)),
  };
}

/**
 * Encodes a standalone unpacked Cap'n `ExecuteReply` message.
 *
 * @param reply Structured reply.
 * @returns Unpacked Cap'n stream bytes.
 */
export function encodeExecuteReply(reply: ExecuteReply): Uint8Array {
  const msg = new CapnpMessage();
  const root = msg.initRoot(0, 3);
  writeExecuteReply(root, reply);
  return msg.finish();
}

/**
 * Encodes `ExecuteResultReply` (`ok` or `err`).
 *
 * @param outcome Successful reply or a wire `PluginError`.
 * @returns Unpacked Cap'n stream bytes.
 */
export function encodeExecuteResultReply(
  outcome: { ok: ExecuteReply } | { err: { code: string; message: string; details?: string } },
): Uint8Array {
  const msg = new CapnpMessage();
  const root = msg.initRoot(1, 1);
  if ("ok" in outcome) {
    root.setUint16(0, 0);
    writeExecuteReply(root.initStruct(0, 0, 3), outcome.ok);
  } else {
    root.setUint16(0, 1);
    const err = root.initStruct(0, 0, 3);
    err.setText(0, outcome.err.code);
    err.setText(1, outcome.err.message);
    err.setText(2, outcome.err.details ?? "");
  }
  return msg.finish();
}

/**
 * Decodes a standalone `ExecuteResultReply`. `err` is thrown as `PluginError`.
 *
 * @param bytes Unpacked Cap'n stream.
 * @returns Structured `ExecuteReply`.
 * @throws When the union is `err` or the buffer is invalid.
 */
export function decodeExecuteResultReply(bytes: Uint8Array): ExecuteReply {
  const root = new CapnpReader(bytes).root(1, 1);
  const disc = root.getUint16(0);
  if (disc === 0) {
    return readExecuteReply(root.getStruct(0, 0, 3));
  }
  if (disc === 1) {
    const err = root.getStruct(0, 0, 3);
    throw PluginError.fromWire(err.getText(0), err.getText(1), err.getText(2));
  }
  throw new Error("unknown ExecuteResultReply union member");
}

/**
 * Encodes a standalone `DbCapabilitiesReply` (`ok` or `err`), the answer of
 * `GuestDatabase.limits` on the granted channel (`GET /db/limits`).
 *
 * @param outcome Capability advertisement or a wire `PluginError`.
 * @returns Unpacked Cap'n stream bytes.
 */
export function encodeDbCapabilitiesReply(
  outcome: { ok: DbCapabilities } | { err: { code: string; message: string; details?: string } },
): Uint8Array {
  return encodeMessage(
    DbCapabilitiesReplyCodec,
    "ok" in outcome
      ? { kind: "ok", value: outcome.ok }
      : {
          kind: "err",
          value: {
            code: outcome.err.code,
            message: outcome.err.message,
            details: outcome.err.details ?? "",
          },
        },
  );
}

/**
 * Decodes a standalone `DbCapabilitiesReply`. `err` is thrown as `PluginError`.
 *
 * @param bytes Unpacked Cap'n stream.
 * @returns The session's `DbCapabilities`.
 * @throws When the union is `err` or the buffer is invalid.
 */
export function decodeDbCapabilitiesReply(bytes: Uint8Array): DbCapabilities {
  const reply = decodeMessage(DbCapabilitiesReplyCodec, bytes);
  if (reply.kind === "ok") {
    return reply.value;
  }
  throw PluginError.fromWire(reply.value.code, reply.value.message, reply.value.details);
}

/**
 * Maps one typed statement result to Cloudflare {@link D1Result}.
 *
 * @param stmt Per-statement rows, columns, and `rowsAffected`.
 * @param timing Handler/engine timing from the execute reply.
 * @returns Cloudflare-shaped `{ success, results, meta }`.
 */
export function statementResultToD1Result(stmt: StatementResult, timing: DbTiming): D1Result {
  const changes = stmt.rowsAffected;
  const durationMs = timing.dbExecutionUs / 1000;
  const results =
    stmt.columns.length > 0
      ? stmt.rows.map((row) => {
          const obj: Record<string, DbValue> = {};
          for (let i = 0; i < stmt.columns.length; i++) {
            obj[stmt.columns[i].name] = row.values[i];
          }
          return obj;
        })
      : null;
  return {
    success: true,
    results,
    meta: {
      duration: durationMs,
      changes,
      last_row_id: 0,
      changed_db: changes > 0,
      rows_read: stmt.rows.length,
      rows_written: changes,
    },
  };
}

/**
 * Maps a typed execute reply to Cloudflare {@link D1Result} per statement.
 *
 * @param reply Decoded `ExecuteReply` from the host transport.
 * @returns One `D1Result` per statement, in plan order.
 */
export function executeReplyToD1Results(reply: ExecuteReply): D1Result[] {
  return reply.statements.map((stmt) => statementResultToD1Result(stmt, reply.timing));
}

function rowMapFromStatement(result: StatementResult): Record<string, DbValue> | null {
  if (result.rows.length === 0) {
    return null;
  }
  const row: Record<string, DbValue> = {};
  for (let i = 0; i < result.columns.length; i++) {
    row[result.columns[i].name] = result.rows[0].values[i];
  }
  return row;
}

function columnValueFromRow(row: Record<string, DbValue>, colName: string): DbValue {
  if (colName in row) {
    return row[colName]!;
  }
  const lower = colName.toLowerCase();
  for (const [name, value] of Object.entries(row)) {
    if (name.toLowerCase() === lower) {
      return value;
    }
  }
  throw new Error(`column ${colName} not found in first() result`);
}

function writeExecuteReply(root: CapnpStruct, reply: ExecuteReply): void {
  root.setText(0, reply.operationId);
  const stmts = root.initStructList(1, reply.statements.length, 1, 2);
  for (let i = 0; i < reply.statements.length; i++) {
    writeStatementResult(stmts[i], reply.statements[i]);
  }
  const timing = root.initStruct(2, 2, 1);
  timing.setUint64(0, BigInt(reply.timing.attemptElapsedUs));
  timing.setUint64(1, BigInt(reply.timing.dbExecutionUs));
  timing.setText(0, reply.timing.dbTimingSource);
}

function readExecuteReply(root: StructReader): ExecuteReply {
  return {
    operationId: root.getText(0),
    statements: root.getStructList(1, 1, 2).map(readStatementResult),
    timing: (() => {
      const t = root.getStruct(2, 2, 1);
      return {
        attemptElapsedUs: Number(t.getUint64(0)),
        dbExecutionUs: Number(t.getUint64(1)),
        dbTimingSource: t.getText(0),
      };
    })(),
  };
}

function writeStatementResult(s: CapnpStruct, stmt: StatementResult): void {
  s.setUint64(0, BigInt(stmt.rowsAffected));
  const rows = s.initStructList(0, stmt.rows.length, 0, 1);
  for (let i = 0; i < stmt.rows.length; i++) {
    const cells = rows[i].initStructList(0, stmt.rows[i].values.length, 2, 1);
    for (let j = 0; j < stmt.rows[i].values.length; j++) {
      writeDbValue(cells[j], stmt.rows[i].values[j]);
    }
  }
  const cols = s.initStructList(1, stmt.columns.length, 1, 1);
  for (let i = 0; i < stmt.columns.length; i++) {
    cols[i].setText(0, stmt.columns[i].name);
    cols[i].setUint16(0, COL_TYPE_ORD[stmt.columns[i].dbType]);
  }
}

function readStatementResult(s: StructReader): StatementResult {
  const columns = s.getStructList(1, 1, 1).map((c) => {
    const ty = COL_TYPE_FROM[c.getUint16(0)];
    if (ty === undefined) {
      throw new Error("unknown DbType");
    }
    return { name: c.getText(0), dbType: ty };
  });
  const rows = s.getStructList(0, 0, 1).map((row) => ({
    values: row.getStructList(0, 2, 1).map(readDbValue),
  }));
  return {
    rows,
    columns,
    rowsAffected: Number(s.getUint64(0)),
  };
}

/**
 * Builds a host-mediated {@link DatabaseBinding} over an `execute` transport.
 *
 * @param transport Host session projection.
 * @param options Request-budget and idempotency knobs.
 * @returns Binding whose `execute(batch)` encodes Cap'n then calls the host.
 */
export function createDatabaseBinding(
  transport: AtomicTransport,
  options: DatabaseBindingOptions = {},
): DatabaseBinding {
  // The session's limits, fetched once per binding (one invocation). `null`
  // when the host predates `limits()` or the call failed: the binding then
  // reads inline and lets `limits()` report the error itself.
  let cachedLimits: Promise<DbCapabilities | null> | undefined;
  const sessionLimits = (): Promise<DbCapabilities | null> => {
    if (cachedLimits === undefined) {
      try {
        cachedLimits =
          transport.limits === undefined
            ? Promise.resolve(null)
            : transport.limits().catch(() => null);
      } catch {
        // A transport that throws synchronously is read inline like one
        // whose promise rejects.
        cachedLimits = Promise.resolve(null);
      }
    }
    return cachedLimits;
  };

  const requestFor = async (
    batch: TypedDbStatement[],
    retry?: RetryToken,
  ): Promise<ExecuteRequest> => {
    if (!Array.isArray(batch) || batch.length === 0) {
      throw new Error("execute statements must be non-empty");
    }
    const request = await executeRequestFromBatch(batch, options, retry);
    const encoded = encodeExecuteRequest(request);
    // The tighter of the binding's configured cap and the session's, so an
    // oversized request fails locally with the typed error the host would
    // raise (the same cap `chunk()` splits to).
    const cap = tighterCap(
      options.maxRequestBytes ?? 0,
      (await sessionLimits())?.maxRequestBytes ?? 0,
    );
    if (cap > 0 && encoded.byteLength > cap) {
      throw PluginError.limitExceeded({
        limit: "maxRequestBytes",
        cap,
        used: encoded.byteLength,
      });
    }
    return request;
  };

  // Opens the streamed read for `request` when the transport and the session
  // both stream; `undefined` means read inline. An `unsupported` answer (an
  // older host, or a session that advertised streaming it cannot serve)
  // also falls back to inline.
  const openStreamed = async (
    request: ExecuteRequest,
  ): Promise<{ chunks: AsyncIterable<Uint8Array>; caps: DbCapabilities } | undefined> => {
    if (transport.executeStreamed === undefined) {
      return undefined;
    }
    const caps = await sessionLimits();
    if (caps === null || !streamsResults(caps)) {
      return undefined;
    }
    let body: ReadableStream<Uint8Array> | Uint8Array;
    try {
      body = await transport.executeStreamed(request);
    } catch (err) {
      if (err instanceof PluginError && err.wireCode === "unsupported") {
        return undefined;
      }
      throw err;
    }
    const chunks =
      body instanceof Uint8Array
        ? (async function* one(): AsyncIterable<Uint8Array> {
            yield body;
          })()
        : streamChunks(body);
    return { chunks, caps };
  };

  const runExecute = async (
    batch: TypedDbStatement[],
    retry?: RetryToken,
  ): Promise<ExecuteReply> => {
    const request = await requestFor(batch, retry);
    const streamed = await openStreamed(request);
    if (streamed === undefined) {
      return replyFor(request, await transport.execute(request));
    }
    const decoder = new ExecuteStreamDecoder(executeStreamLimits(streamed.caps));
    for await (const chunk of streamed.chunks) {
      decoder.push(chunk);
    }
    let reply: ExecuteReply;
    try {
      reply = decoder.finish();
    } catch (err) {
      if (err instanceof PluginError && err.wireCode === "unsupported") {
        return replyFor(request, await transport.execute(request));
      }
      throw err;
    }
    return replyFor(request, reply);
  };

  // Rows of a single-statement request read inline, from one `execute`.
  const inlineRows = async function* (
    request: ExecuteRequest,
  ): AsyncIterable<Record<string, DbValue>> {
    const reply = replyFor(request, await transport.execute(request));
    const result = reply.statements[0];
    if (result) {
      for (const row of result.rows) {
        yield rowMap(result.columns, row);
      }
    }
  };

  // Rows of a single-statement request as they arrive, without keeping them.
  // A stream that answers `unsupported` before any row arrived falls back to
  // the inline read like `all()`; once rows were handed out the error is the
  // caller's, since the read cannot restart behind them.
  const iterateRows = async function* (
    batch: TypedDbStatement[],
    retry?: RetryToken,
  ): AsyncIterable<Record<string, DbValue>> {
    const request = await requestFor(batch, retry);
    const streamed = await openStreamed(request);
    if (streamed === undefined) {
      yield* inlineRows(request);
      return;
    }
    const decoder = new ExecuteStreamDecoder(executeStreamLimits(streamed.caps), {
      retainRows: false,
    });
    const fallsBackInline = (err: unknown, yielded: boolean): boolean =>
      !yielded && err instanceof PluginError && err.wireCode === "unsupported";
    // One push can complete the statement, its rows and the end frame at
    // once, so the columns come from the frames rather than the decoder.
    let columns: DbColumn[] = [];
    let yielded = false;
    for await (const chunk of streamed.chunks) {
      let frames: ExecuteStreamFrame[];
      try {
        frames = decoder.push(chunk);
      } catch (err) {
        if (fallsBackInline(err, yielded)) {
          yield* inlineRows(request);
          return;
        }
        throw err;
      }
      for (const frame of frames) {
        if (frame.kind === "head") {
          checkReplyIdentity(request, frame.value.operationId, frame.value.statementCount);
        } else if (frame.kind === "statement") {
          columns = frame.value.columns;
        } else if (frame.kind === "rows") {
          for (const row of frame.value) {
            yielded = true;
            yield rowMap(columns, row);
          }
        }
      }
    }
    try {
      decoder.finish();
    } catch (err) {
      if (fallsBackInline(err, yielded)) {
        yield* inlineRows(request);
        return;
      }
      throw err;
    }
  };

  const binding: DatabaseBinding = {
    prepare(sql: string): PreparedStatement {
      return makePrepared(binding, sql, [], options.maxResultRows ?? 0, defaultIntent(options));
    },
    batch(statements, opts) {
      const typed = statements.map((s) => (s as PreparedInternal)._asTyped());
      return runExecute(typed, opts?.retry).then(executeReplyToD1Results);
    },
    exec(query, opts) {
      const queries = splitExecQueries(query);
      if (queries.length === 0) {
        return Promise.reject(new Error("exec query is empty"));
      }
      const prepared = queries.map((sql) => binding.prepare(sql));
      return binding.batch(prepared, opts).then((results) => ({
        count: results.length,
        duration: results.reduce((sum, r) => sum + r.meta.duration, 0),
      }));
    },
    execute(batch, opts) {
      return runExecute(batch, opts?.retry);
    },
    limits() {
      if (transport.limits === undefined) {
        return Promise.reject(
          PluginError.fromWire("unsupported", "GuestDatabase.limits is not available on this host"),
        );
      }
      return sessionLimits().then(async (caps) => {
        if (caps !== null) {
          return caps;
        }
        // The cached fetch failed: let the host's own error surface, and
        // keep a later success so reads use the session's budget.
        const fetched = await (transport.limits as () => Promise<DbCapabilities>)();
        cachedLimits = Promise.resolve(fetched);
        return fetched;
      });
    },
    async *chunk(statements, opts) {
      const explicit = opts?.maxStatements !== undefined && opts.maxRequestBytes !== undefined;
      const caps = explicit ? null : await sessionLimits();
      const maxStatements = opts?.maxStatements ?? caps?.maxStatements ?? 0;
      // The session's request cap and the binding's own both apply to
      // `batch()`, so a batch is sized to the tighter one.
      const maxRequestBytes =
        opts?.maxRequestBytes ??
        tighterCap(caps?.maxRequestBytes ?? 0, options.maxRequestBytes ?? 0);
      // Explicit zero caps mean "one batch"; only missing caps are an error.
      if (!explicit && maxStatements <= 0 && maxRequestBytes <= 0) {
        throw PluginError.fromWire(
          "unsupported",
          "chunk needs the session limits (GuestDatabase.limits) or explicit maxStatements / maxRequestBytes",
        );
      }
      const typed = statements.map((stmt) => (stmt as PreparedInternal)._asTyped());
      for (const indexes of chunkIndexes(typed, maxStatements, maxRequestBytes)) {
        yield indexes.map((i) => statements[i]!);
      }
    },
  };
  // `iterate()` needs the binding's streamed path; `makePrepared` reaches it
  // through this hook so prepared statements stay plain objects.
  (binding as BindingInternal)._iterateRows = iterateRows;
  return binding;
}

interface BindingInternal extends DatabaseBinding {
  _iterateRows?(
    batch: TypedDbStatement[],
    retry?: RetryToken,
  ): AsyncIterable<Record<string, DbValue>>;
}

/**
 * The smaller of two caps where `0` means "no cap".
 *
 * @param a - One cap.
 * @param b - Another cap.
 * @returns The tighter cap, or the one that is set, or `0`.
 */
function tighterCap(a: number, b: number): number {
  if (a > 0 && b > 0) {
    return Math.min(a, b);
  }
  return a > 0 ? a : b;
}

/**
 * Checks that a reply answers `request`: the same `operationId` (a request
 * sent without one takes the id the host minted) and one result per
 * statement.
 *
 * @param request - The request sent.
 * @param operationId - The reply's operation id.
 * @param statementCount - Results the reply carries or announces.
 * @throws `PluginError` `invalid_params` when the reply is for another request.
 */
function checkReplyIdentity(
  request: ExecuteRequest,
  operationId: string,
  statementCount: number,
): void {
  if (request.operationId !== "" && operationId !== request.operationId) {
    throw PluginError.fromWire(
      "invalid_params",
      `reply answers operation ${operationId}, not ${request.operationId}`,
    );
  }
  if (statementCount !== request.statements.length) {
    throw PluginError.fromWire(
      "invalid_params",
      `reply carries ${statementCount} results for ${request.statements.length} statements`,
    );
  }
}

/**
 * `reply` once it is checked against `request`: its identity, and no result
 * with more rows than its statement's `maxRows` asked for.
 *
 * @param request - The request sent.
 * @param reply - The reply received.
 * @returns `reply`.
 * @throws See {@link checkReplyIdentity}; `PluginError` `invalid_params`
 *   when a result carries more rows than its statement allowed.
 */
function replyFor(request: ExecuteRequest, reply: ExecuteReply): ExecuteReply {
  checkReplyIdentity(request, reply.operationId, reply.statements.length);
  for (let i = 0; i < reply.statements.length; i++) {
    const maxRows = request.statements[i]?.maxRows ?? 0;
    const rows = reply.statements[i]?.rows.length ?? 0;
    if (maxRows > 0 && rows > maxRows) {
      throw PluginError.fromWire(
        "invalid_params",
        `reply carries ${rows} rows for statement ${i}, which asked for at most ${maxRows}`,
      );
    }
  }
  return reply;
}

function rowMap(columns: DbColumn[], row: DbRow): Record<string, DbValue> {
  const out: Record<string, DbValue> = {};
  for (let i = 0; i < columns.length; i++) {
    const value = row.values[i];
    if (value !== undefined) {
      out[columns[i]!.name] = value;
    }
  }
  return out;
}

/**
 * Greedy split of `typed` into index runs whose encoded request stays within
 * `maxRequestBytes` and whose length stays within `maxStatements` (`0` lifts
 * a cap). Sizes are estimated per statement and each run is verified by one
 * exact encode; a run the estimate let through is halved until it fits, and
 * a lone statement over the cap is yielded on its own.
 *
 * @param typed Statements to split.
 * @param maxStatements Statements per request (`0` = no cap).
 * @param maxRequestBytes Encoded request bytes (`0` = no cap).
 * @returns Index runs in order, covering every statement once.
 */
export function chunkIndexes(
  typed: TypedDbStatement[],
  maxStatements: number,
  maxRequestBytes: number,
): number[][] {
  if (typed.length === 0) {
    return [];
  }
  // Requests never encode empty, so the fixed cost of a request and the
  // cost of each statement are measured against one probe statement.
  const base =
    encodeExecuteRequest({
      operationId: RETRY_ID_PLACEHOLDER,
      requestHash: RETRY_HASH_PLACEHOLDER,
      statements: [PROBE_STATEMENT],
      deadlineUnixMs: 0,
    }).byteLength - PROBE_STATEMENT_BYTES;
  const sizes = typed.map(
    (stmt) =>
      encodeExecuteRequest({
        operationId: "",
        requestHash: "",
        statements: [PROBE_STATEMENT, stmt],
        deadlineUnixMs: 0,
      }).byteLength - PROBE_REQUEST_BYTES,
  );
  const fits = (run: number[]): boolean =>
    maxRequestBytes <= 0 ||
    encodeExecuteRequest({
      operationId: RETRY_ID_PLACEHOLDER,
      requestHash: RETRY_HASH_PLACEHOLDER,
      statements: run.map((i) => typed[i]!),
      deadlineUnixMs: 0,
    }).byteLength <= maxRequestBytes;
  const out: number[][] = [];
  const emit = (run: number[]): void => {
    if (run.length <= 1 || fits(run)) {
      out.push(run);
      return;
    }
    const half = Math.ceil(run.length / 2);
    emit(run.slice(0, half));
    emit(run.slice(half));
  };
  let run: number[] = [];
  let bytes = base;
  for (let i = 0; i < typed.length; i++) {
    const size = sizes[i]!;
    const overStatements = maxStatements > 0 && run.length >= maxStatements;
    const overBytes = maxRequestBytes > 0 && run.length > 0 && bytes + size > maxRequestBytes;
    if (overStatements || overBytes) {
      emit(run);
      run = [];
      bytes = base;
    }
    run.push(i);
    bytes += size;
  }
  if (run.length > 0) {
    emit(run);
  }
  return out;
}

/** A UUID-shaped `operationId`, the widest the binding mints. */
const RETRY_ID_PLACEHOLDER = "00000000-0000-4000-8000-000000000000";
/** A SHA-256 hex `requestHash`, the widest a retry token carries. */
const RETRY_HASH_PLACEHOLDER = "0".repeat(64);
/** The smallest statement a request can carry, used to measure the others. */
const PROBE_STATEMENT: TypedDbStatement = {
  sql: "",
  parameters: [],
  kind: "select",
  maxRows: 0,
  resultSelection: "rows",
};
/** Encoded bytes of a request with empty ids and one probe statement. */
const PROBE_REQUEST_BYTES = encodeExecuteRequest({
  operationId: "",
  requestHash: "",
  statements: [PROBE_STATEMENT],
  deadlineUnixMs: 0,
}).byteLength;
/** Encoded bytes one probe statement adds to a request. */
const PROBE_STATEMENT_BYTES =
  encodeExecuteRequest({
    operationId: "",
    requestHash: "",
    statements: [PROBE_STATEMENT, PROBE_STATEMENT],
    deadlineUnixMs: 0,
  }).byteLength - PROBE_REQUEST_BYTES;

interface TerminalIntent {
  resultSelection: DbResultSelection;
  maxRows: number;
}

interface PreparedInternal extends PreparedStatement {
  _intent?: TerminalIntent;
  _asTyped(): TypedDbStatement;
}

function defaultIntent(options: DatabaseBindingOptions): TerminalIntent {
  return {
    resultSelection: "rows",
    maxRows: options.maxResultRows ?? 0,
  };
}

function makePrepared(
  binding: DatabaseBinding,
  sql: string,
  parameters: DbValue[],
  defaultAllRows: number,
  intent?: TerminalIntent,
): PreparedInternal {
  const stmt: PreparedInternal = {
    _intent: intent,
    bind(...values: DbValue[]) {
      return makePrepared(binding, sql, values, defaultAllRows, intent);
    },
    asRun() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "affectedRows",
        maxRows: 0,
      });
    },
    asFirst() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "rows",
        maxRows: 1,
      });
    },
    asAll() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "rows",
        maxRows: defaultAllRows,
      });
    },
    run(options) {
      return binding
        .execute([(this.asAll() as PreparedInternal)._asTyped()], options)
        .then((reply) => statementResultToD1Result(reply.statements[0]!, reply.timing));
    },
    first(colName?: string, options?: { retry?: RetryToken }) {
      return binding
        .execute([(this.asFirst() as PreparedInternal)._asTyped()], options)
        .then((reply) => {
          const result = reply.statements[0];
          if (!result) {
            return null;
          }
          const row = rowMapFromStatement(result);
          if (row === null) {
            return null;
          }
          if (colName !== undefined) {
            return columnValueFromRow(row, colName);
          }
          return row;
        });
    },
    raw(options) {
      return binding
        .execute([(this.asAll() as PreparedInternal)._asTyped()], options)
        .then((reply) => {
          const result = reply.statements[0];
          if (!result) {
            return [];
          }
          return result.rows.map((row) => row.values);
        });
    },
    all(options) {
      return binding
        .execute([(this.asAll() as PreparedInternal)._asTyped()], options)
        .then((reply) => statementResultToD1Result(reply.statements[0]!, reply.timing));
    },
    iterate(options) {
      const typed = [(this.asAll() as PreparedInternal)._asTyped()];
      const streamedRows = (binding as BindingInternal)._iterateRows;
      if (streamedRows !== undefined) {
        return streamedRows(typed, options?.retry);
      }
      return (async function* inline(): AsyncIterable<Record<string, DbValue>> {
        const reply = await binding.execute(typed, options);
        const result = reply.statements[0];
        if (result) {
          for (const row of result.rows) {
            yield rowMap(result.columns, row);
          }
        }
      })();
    },
    _asTyped(): TypedDbStatement {
      const used = intent ?? defaultIntent({ maxResultRows: defaultAllRows });
      return {
        sql,
        parameters,
        kind:
          used.resultSelection === "affectedRows" || used.resultSelection === "discard"
            ? "execute"
            : guestStatementKind(sql),
        maxRows: used.maxRows,
        resultSelection: used.resultSelection,
      };
    },
  };
  return stmt;
}

async function executeRequestFromBatch(
  batch: TypedDbStatement[],
  options: DatabaseBindingOptions,
  retry?: RetryToken,
): Promise<ExecuteRequest> {
  const token = retry ?? options.retry;
  return {
    operationId: token?.operationId ?? options.operationId ?? newOperationId(),
    requestHash: token?.requestHash ?? options.requestHash ?? "",
    statements: batch,
    deadlineUnixMs: options.deadlineUnixMs ?? 0,
  };
}

/**
 * SHA-256 hex of the Cap'n request with id and hash cleared.
 *
 * @param request Structured request.
 * @returns Hex digest matching the host canonical hash.
 */
export async function canonicalExecuteRequestHash(request: ExecuteRequest): Promise<string> {
  const canonical: ExecuteRequest = {
    ...request,
    operationId: "",
    requestHash: "",
    deadlineUnixMs: 0,
  };
  // Copy into a fresh ArrayBuffer: WebCrypto's BufferSource rejects views that
  // may be backed by a SharedArrayBuffer.
  const bytes = new Uint8Array(encodeExecuteRequest(canonical));
  if (globalThis.crypto?.subtle) {
    const digest = await globalThis.crypto.subtle.digest("SHA-256", bytes);
    return hexBytes(new Uint8Array(digest));
  }
  const { createHash } = await import("node:crypto");
  return createHash("sha256").update(bytes).digest("hex");
}

function hexBytes(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function newOperationId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `op-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function writeStatement(s: CapnpStruct, stmt: TypedDbStatement): void {
  s.setText(0, stmt.sql);
  s.setUint16(0, KIND_ORD[stmt.kind]);
  s.setUint16(1, SELECT_ORD[stmt.resultSelection]);
  s.setUint32(1, stmt.maxRows);
  const params = s.initStructList(1, stmt.parameters.length, 2, 1);
  for (let i = 0; i < stmt.parameters.length; i++) {
    writeDbValue(params[i], stmt.parameters[i]);
  }
}

function readStatement(s: StructReader): TypedDbStatement {
  const kind = KIND_FROM[s.getUint16(0)];
  const selectionRaw = s.getUint16(1);
  const selection = SELECT_FROM[selectionRaw] ?? "rows";
  if (kind === undefined) {
    throw new Error("unknown DbStatementKind");
  }
  return {
    sql: s.getText(0),
    parameters: s.getStructList(1, 2, 1).map(readDbValue),
    kind,
    maxRows: s.getUint32(1),
    resultSelection: selection,
  };
}
