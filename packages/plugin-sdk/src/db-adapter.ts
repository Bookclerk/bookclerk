/**
 * Adapter-side batch runner for isolate database adapters.
 *
 * A guest's `executeAtomic` reaches a database adapter wrapped in a durable
 * replay receipt: the host puts a prune `DELETE` and a prior-receipt `SELECT`
 * in front of the guest statements, gates every guest write on the receipt
 * row being absent, and appends a stub `INSERT` that claims the receipt. The
 * adapter's part is to persist the guest reply on that receipt **before**
 * `COMMIT`, so that a retry with the same `operationId` after a lost reply
 * replays the recorded result instead of re-running the writes.
 *
 * {@link executeAdapterBatch} and {@link executeAdapterBatchStreamed} do that
 * part once for every isolate adapter: they run the wrapped statements on the
 * adapter's transaction, skip the guest work a completed prior receipt
 * already covers, run the finalize `UPDATE` that stores the replay payload,
 * and commit only when the reply can be delivered. An isolate adapter's
 * streamed frames cross the invoke reply as one scalar, so the streamed
 * variant measures the encoded frames against `MAX_SCALAR_BYTES` before it
 * commits and rolls the batch back with `limit_exceeded` when they do not
 * fit: a reply the host can never receive never commits.
 *
 * The receipt logic mirrors `bookclerk_db_exec::guest_receipt` in the host,
 * and the payload is the same JSON envelope the host decodes on replay.
 */

import { MAX_SCALAR_BYTES } from "./abi.js";
import {
  encodeExecuteReply,
  type DbColumn,
  type DbRow,
  type ExecuteReply,
  type StatementResult,
} from "./db-execute.js";
import type { DbValue } from "./db-value.js";
import { decodeExecuteStream, validateCell, type ExecuteStreamLimits } from "./db-stream.js";
import { PluginError } from "./errors.js";
import type { AdapterReceipt, AdapterStatement, ExecuteStreamFrame } from "./generated.js";
import { ExecuteStreamFrameCodec, encodeMessage } from "./generated-wire.js";

/** Prune + prior-receipt `SELECT` statements ahead of the guest statements. */
export const GUEST_RECEIPT_WRAP_PREFIX = 2;

/** Receipt stub `INSERT` after the guest statements. */
export const GUEST_RECEIPT_STUB_SUFFIX = 1;

/** Largest replay payload, in UTF-8 bytes, a receipt stores for a write batch. */
export const GUEST_REPLAY_PAYLOAD_MAX_BYTES = 65536;

/**
 * Payload stored for a read-only guest batch instead of its rows: reads are
 * not gated, so they run again on replay and need no recorded result.
 */
export const GUEST_READ_ONLY_REPLAY_PAYLOAD = '{"readOnly":true}';

/** Error text when a receipt committed but the reply it should hold was lost. */
export const GUEST_RECEIPT_RESULT_LOST =
  "guest receipt committed without payload; original response lost";

/**
 * `unavailable` message when a concurrent attempt committed the receipt
 * between this attempt's prior-receipt `SELECT` and its stub `INSERT`.
 */
export const GUEST_RECEIPT_CLAIM_LOST =
  "guest receipt claimed by a concurrent attempt; retry to replay it";

/** Encoded size an encoder aims for per `rows` frame (one wider row may exceed it). */
export const STREAM_FRAME_TARGET_BYTES = 64 * 1024;

/** Receipt status of an in-flight claim. */
const STATUS_CLAIMED = "claimed";

/** Finalize `UPDATE` that stores the replay payload on the claimed receipt. */
const FINALIZE_SQL =
  "UPDATE bookclerk_receipts SET payload = ?, status = 'ok' " +
  "WHERE operation_id = ? AND status IN ('claimed', 'applied')";

/** The parts of an {@link AdapterStatement} an adapter executes. */
export type AdapterBatchStatement = Pick<
  AdapterStatement,
  "sql" | "parameters" | "kind" | "maxRows" | "resultSelection"
>;

/** The parts of an `AdapterExecuteRequest` the batch runner reads. */
export interface AdapterBatchRequest {
  /** Host-chosen idempotency key, echoed on the reply and keyed on the receipt. */
  operationId: string;
  /** Wrapped statements in execution order (proofs are not read). */
  statements: AdapterBatchStatement[];
  /** Receipt hint; `guestLen` `0` means the batch carries no guest receipt. */
  receipt: AdapterReceipt;
}

/**
 * One open transaction on the adapter's engine.
 *
 * The runner calls `run` once per statement in order, including the finalize
 * statements it adds (canonical SQL with `?` placeholders, lowered by the
 * adapter like any other statement), then `commit`, or `rollback` when any
 * step failed.
 */
export interface AdapterTransaction {
  /**
   * Runs one statement inside the transaction.
   *
   * @param statement - Canonical statement to lower and execute.
   * @returns The statement's result (rows only when `resultSelection` is `rows`).
   */
  run(statement: AdapterBatchStatement): Promise<StatementResult>;
  /**
   * Commits the transaction.
   *
   * @returns Resolves once the batch is durable.
   */
  commit(): Promise<void>;
  /**
   * Rolls the transaction back.
   *
   * @returns Resolves once nothing of the batch remains.
   */
  rollback(): Promise<void>;
}

/** Options shared by the batch runners. */
export interface AdapterBatchOptions {
  /** Observability source on the reply timing (for example `sqlite_txn`). */
  timingSource: string;
}

/** Options of {@link executeAdapterBatchStreamed}. */
export interface AdapterBatchStreamedOptions extends AdapterBatchOptions {
  /**
   * The streamed budget this adapter advertises
   * (`executeStreamLimits(caps)`); the host decodes the reply under it.
   */
  limits: ExecuteStreamLimits;
}

/**
 * Runs one host batch in a transaction and commits it with its replay receipt.
 *
 * @param request - The adapter request (`execute`).
 * @param begin - Opens a transaction on the adapter's engine.
 * @param options - Timing source.
 * @returns The committed reply, wrap statements included (the host unwraps it).
 * @throws Whatever a statement, the finalize or `commit` raised, after the
 *   transaction was rolled back; `PluginError` `unavailable` when a prior
 *   receipt committed without its reply.
 */
export async function executeAdapterBatch(
  request: AdapterBatchRequest,
  begin: () => Promise<AdapterTransaction>,
  options: AdapterBatchOptions,
): Promise<ExecuteReply> {
  return inTransaction(begin, (txn) => runAdapterBatch(request, txn, options));
}

/**
 * Runs one host batch for `executeStreamed` and commits it only when the
 * host can receive its frames.
 *
 * The frames are encoded inside the transaction. Encoding stops with
 * `limit_exceeded` (`maxScalarBytes`) as soon as they pass one invoke
 * scalar, the finished frames are decoded under `options.limits` the way
 * the host decodes them, and the reassembled reply's encoded size is held to
 * `options.limits.maxBytes` the way the host validates it (the decoder alone
 * allows framing slack past the budget). Any failure rolls the batch back,
 * so nothing commits that the host would refuse.
 *
 * @param request - The adapter request (`executeStreamed`).
 * @param begin - Opens a transaction on the adapter's engine.
 * @param options - Timing source and the advertised streamed budget.
 * @returns Every frame of the committed reply, concatenated.
 * @throws `PluginError` `limit_exceeded` when the frames do not fit one
 *   scalar or the budget, or whatever {@link executeAdapterBatch} throws.
 */
export async function executeAdapterBatchStreamed(
  request: AdapterBatchRequest,
  begin: () => Promise<AdapterTransaction>,
  options: AdapterBatchStreamedOptions,
): Promise<Uint8Array> {
  return inTransaction(begin, async (txn) => {
    const reply = await runAdapterBatch(request, txn, options);
    const frames = encodeExecuteStream(reply, STREAM_FRAME_TARGET_BYTES, MAX_SCALAR_BYTES);
    decodeExecuteStream(frames, options.limits);
    requireReplyWithinBudget(reply, options.limits);
    return frames;
  });
}

/**
 * Holds a reply's encoded `ExecuteReply` size to the streamed budget, as the
 * host does after it reassembles the frames.
 *
 * Each statement result encodes smaller than the whole reply, so this also
 * covers the host's per-statement byte check under the same budget.
 *
 * @param reply - The reply about to commit.
 * @param limits - The advertised streamed budget (`maxBytes` `0` = unlimited,
 *   as the host reads it).
 * @throws `PluginError` `limit_exceeded` naming `limits.bytesLimit` when the
 *   encoded reply is larger than `limits.maxBytes`.
 */
function requireReplyWithinBudget(reply: ExecuteReply, limits: ExecuteStreamLimits): void {
  if (limits.maxBytes === 0) {
    return;
  }
  const used = encodeExecuteReply(reply).length;
  if (used > limits.maxBytes) {
    throw PluginError.limitExceeded({ limit: limits.bytesLimit, cap: limits.maxBytes, used });
  }
}

/**
 * Opens a transaction, runs `body` in it, commits on success and rolls back
 * on any failure.
 *
 * @param begin - Opens the transaction.
 * @param body - Work to run before `COMMIT`.
 * @returns What `body` returned.
 * @throws What `body` or `commit` threw (a failing rollback does not replace it).
 */
async function inTransaction<T>(
  begin: () => Promise<AdapterTransaction>,
  body: (txn: AdapterTransaction) => Promise<T>,
): Promise<T> {
  const txn = await begin();
  let result: T;
  try {
    result = await body(txn);
    await txn.commit();
  } catch (err) {
    try {
      await txn.rollback();
    } catch {
      // The original failure is the one the host needs to see.
    }
    throw err;
  }
  return result;
}

/**
 * Runs the wrapped statements and the receipt finalize on an open
 * transaction, without committing.
 *
 * After the prior-receipt `SELECT`, a completed receipt for this operation
 * skips the remaining guest work (its results are padded so the wrap shape
 * stays intact) and the host replays the stored reply.
 *
 * @param request - The adapter request.
 * @param txn - The open transaction.
 * @param options - Timing source.
 * @returns The reply to deliver once the transaction commits.
 * @throws `PluginError` `unavailable` when a prior receipt committed without
 *   its reply, `internal` when the wrap shape is malformed or the payload is
 *   over {@link GUEST_REPLAY_PAYLOAD_MAX_BYTES}, `invalid_params` when a
 *   result cell is not a valid `DbValue`, or whatever `run` threw.
 */
export async function runAdapterBatch(
  request: AdapterBatchRequest,
  txn: Pick<AdapterTransaction, "run">,
  options: AdapterBatchOptions,
): Promise<ExecuteReply> {
  const started = Date.now();
  const total = request.statements.length;
  const guestLen = request.receipt.guestLen;
  const guestHash = request.receipt.guestHash;
  const statements: StatementResult[] = [];
  for (const statement of request.statements) {
    statements.push(await txn.run(statement));
    if (guestLen > 0 && shouldSkipRemainingGuestWork(statements, total, guestHash)) {
      while (statements.length < total) {
        statements.push(affected(0));
      }
      break;
    }
  }
  const timing = (): ExecuteReply["timing"] => {
    const elapsed = Math.max(0, Date.now() - started) * 1000;
    return {
      attemptElapsedUs: elapsed,
      dbExecutionUs: elapsed,
      dbTimingSource: options.timingSource,
    };
  };
  // The host refuses a non-finite float64 or TEXT with U+0000; refuse it
  // here, before COMMIT, so the write and its receipt roll back.
  for (const result of statements) {
    for (const row of result.rows) {
      row.values.forEach(validateCell);
    }
  }
  if (guestLen > 0) {
    // No prior receipt, yet the stub claimed nothing: another attempt
    // committed this operation's receipt after the prior-receipt SELECT, and
    // the gated writes saw it. Roll back so the host's retry replays it.
    const prior = statements[1];
    const stub = statements[statements.length - 1];
    if (prior !== undefined && prior.rows.length === 0 && stub?.rowsAffected === 0) {
      throw PluginError.fromWire("unavailable", GUEST_RECEIPT_CLAIM_LOST);
    }
    // A replay answers with the recorded timing, so record what ran.
    const partial: ExecuteReply = {
      operationId: request.operationId,
      statements,
      timing: timing(),
    };
    const readOnly = guestBatchIsReadOnly(request.statements, guestLen);
    for (const statement of guestReceiptFinalizeStatements(
      partial,
      guestLen,
      guestHash,
      readOnly,
    )) {
      await txn.run(statement);
    }
  }
  return { operationId: request.operationId, statements, timing: timing() };
}

/**
 * True when every guest statement inside the receipt wrap is a `SELECT`.
 *
 * An empty or out-of-range guest slice is not read-only, so a write batch
 * never skips its replay payload.
 *
 * @param statements - The wrapped statements.
 * @param guestLen - Guest statement count from the receipt hint.
 * @returns Whether the batch only reads.
 */
export function guestBatchIsReadOnly(
  statements: readonly Pick<AdapterBatchStatement, "kind">[],
  guestLen: number,
): boolean {
  const end = GUEST_RECEIPT_WRAP_PREFIX + guestLen;
  if (guestLen === 0 || end > statements.length) {
    return false;
  }
  return statements.slice(GUEST_RECEIPT_WRAP_PREFIX, end).every((s) => s.kind === "select");
}

/**
 * Statements that persist the guest reply on the claimed receipt.
 *
 * Empty when the prior receipt already holds a payload, belongs to a
 * different request (the host reports the conflict), or is an in-flight
 * claim whose stub this attempt did not insert.
 *
 * @param partial - The wrapped reply so far (prefix, guest and stub results).
 * @param guestLen - Guest statement count from the receipt hint.
 * @param guestHash - Guest `requestHash` from the receipt hint.
 * @param readOnly - Whether the guest slice only reads ({@link guestBatchIsReadOnly}).
 * @returns The finalize statements to run before `COMMIT`.
 * @throws `PluginError` `unavailable` when a prior receipt committed without
 *   its reply, `internal` when the wrap shape is malformed or the payload is
 *   over {@link GUEST_REPLAY_PAYLOAD_MAX_BYTES}.
 */
export function guestReceiptFinalizeStatements(
  partial: ExecuteReply,
  guestLen: number,
  guestHash: string,
  readOnly: boolean,
): AdapterBatchStatement[] {
  const expected = GUEST_RECEIPT_WRAP_PREFIX + guestLen + GUEST_RECEIPT_STUB_SUFFIX;
  if (partial.statements.length !== expected) {
    throw PluginError.fromWire(
      "internal",
      `guest atomic receipt wrap returned ${partial.statements.length} statements; expected ${expected}`,
    );
  }
  const prior = partial.statements[1];
  const priorHash = prior ? receiptHash(prior) : undefined;
  if (prior && priorHash !== undefined) {
    if (priorHash !== guestHash) {
      return [];
    }
    if (receiptText(prior, "payload", 3) !== undefined) {
      return [];
    }
    const claimed = receiptText(prior, "status", 2) === STATUS_CLAIMED;
    const stubAffected = partial.statements[partial.statements.length - 1]?.rowsAffected ?? 0;
    if (!claimed && stubAffected === 0) {
      throw PluginError.fromWire("unavailable", GUEST_RECEIPT_RESULT_LOST);
    }
    if (claimed && stubAffected === 0) {
      return [];
    }
  }
  const payload = readOnly
    ? GUEST_READ_ONLY_REPLAY_PAYLOAD
    : guestReplayPayload({
        operationId: partial.operationId,
        statements: partial.statements.slice(
          GUEST_RECEIPT_WRAP_PREFIX,
          GUEST_RECEIPT_WRAP_PREFIX + guestLen,
        ),
        timing: partial.timing,
      });
  return [
    {
      sql: FINALIZE_SQL,
      parameters: [
        { kind: "text", value: payload },
        { kind: "text", value: partial.operationId },
      ],
      kind: "execute",
      maxRows: 0,
      resultSelection: "affectedRows",
    },
  ];
}

/**
 * True when the prefix results show a completed prior receipt, so the guest
 * statements must not run again.
 *
 * An in-flight claim with the same hash and a read-only receipt with the
 * same hash do not skip; a receipt with a different hash does (the host
 * reports the conflict).
 *
 * @param results - Results so far.
 * @param total - Wrapped statement count.
 * @param guestHash - Guest `requestHash` from the receipt hint.
 * @returns Whether to pad the remaining results instead of running them.
 */
export function shouldSkipRemainingGuestWork(
  results: readonly StatementResult[],
  total: number,
  guestHash: string,
): boolean {
  if (results.length !== GUEST_RECEIPT_WRAP_PREFIX || total <= GUEST_RECEIPT_WRAP_PREFIX) {
    return false;
  }
  const prior = results[1];
  const priorHash = prior ? receiptHash(prior) : undefined;
  if (!prior || priorHash === undefined) {
    return false;
  }
  const sameHash = priorHash === guestHash;
  const resumesClaim = sameHash && receiptText(prior, "status", 2) === STATUS_CLAIMED;
  const readOnlyReplay =
    sameHash && receiptText(prior, "payload", 3) === GUEST_READ_ONLY_REPLAY_PAYLOAD;
  return !resumesClaim && !readOnlyReplay;
}

/**
 * The replay payload for a guest reply: the JSON envelope the host decodes,
 * `{"operation_id", "statements", "timing"}` with each value in the host's
 * serde form.
 *
 * @param reply - The guest slice of the reply.
 * @returns The payload text.
 * @throws `PluginError` `internal` when a cell does not encode or the
 *   payload is over {@link GUEST_REPLAY_PAYLOAD_MAX_BYTES}.
 */
export function guestReplayPayload(reply: ExecuteReply): string {
  const statements = reply.statements.map(statementJson).join(",");
  const t = reply.timing;
  const text =
    `{"operation_id":${JSON.stringify(reply.operationId)},"statements":[${statements}],` +
    `"timing":{"attemptElapsedUs":${uint(t.attemptElapsedUs)},` +
    `"dbExecutionUs":${uint(t.dbExecutionUs)},` +
    `"dbTimingSource":${JSON.stringify(t.dbTimingSource)}}}`;
  const bytes = new TextEncoder().encode(text).byteLength;
  if (bytes > GUEST_REPLAY_PAYLOAD_MAX_BYTES) {
    throw PluginError.fromWire(
      "internal",
      `guest replay payload is ${bytes} bytes; max is ${GUEST_REPLAY_PAYLOAD_MAX_BYTES}`,
    );
  }
  return text;
}

/**
 * Frames `reply` as the byte sequence `executeStreamed` answers: `head`,
 * each statement with its rows split near `frameTargetBytes`, then `end`. A
 * slice whose frame still exceeds one scalar is halved until each frame fits
 * or holds one row.
 *
 * @param reply - The committed reply.
 * @param frameTargetBytes - Estimated encoded size to aim for per `rows` frame.
 * @param maxBytes - Stop as soon as the frames pass this many bytes.
 * @returns Every frame, concatenated.
 * @throws `PluginError` `limit_exceeded` (`maxScalarBytes`) when the frames
 *   pass `maxBytes`.
 */
export function encodeExecuteStream(
  reply: ExecuteReply,
  frameTargetBytes: number = STREAM_FRAME_TARGET_BYTES,
  maxBytes: number = Number.POSITIVE_INFINITY,
): Uint8Array {
  const parts: Uint8Array[] = [];
  let total = 0;
  const push = (part: Uint8Array): void => {
    total += part.byteLength;
    if (total > maxBytes) {
      throw PluginError.limitExceeded({ limit: "maxScalarBytes", cap: maxBytes, used: total });
    }
    parts.push(part);
  };
  push(
    frame({
      kind: "head",
      value: { operationId: reply.operationId, statementCount: reply.statements.length },
    }),
  );
  reply.statements.forEach((stmt, index) => {
    push(
      frame({
        kind: "statement",
        value: { index, columns: stmt.columns, rowsAffected: stmt.rowsAffected },
      }),
    );
    let start = 0;
    let size = 0;
    stmt.rows.forEach((row, i) => {
      const estimate = rowSizeEstimate(row);
      if (size > 0 && size + estimate > frameTargetBytes) {
        pushRowsFrames(push, stmt.rows.slice(start, i));
        start = i;
        size = 0;
      }
      size += estimate;
    });
    pushRowsFrames(push, stmt.rows.slice(start));
  });
  push(frame({ kind: "end", value: reply.timing }));
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.byteLength;
  }
  return out;
}

/**
 * Encodes `rows` as one or more `rows` frames, halving a slice whose frame
 * exceeds one scalar until each fits or holds one row.
 *
 * @param push - Appends one frame.
 * @param rows - Rows of the open statement.
 */
function pushRowsFrames(push: (part: Uint8Array) => void, rows: DbRow[]): void {
  if (rows.length === 0) {
    return;
  }
  const encoded = frame({ kind: "rows", value: rows });
  if (encoded.byteLength > MAX_SCALAR_BYTES && rows.length > 1) {
    const mid = Math.floor(rows.length / 2);
    pushRowsFrames(push, rows.slice(0, mid));
    pushRowsFrames(push, rows.slice(mid));
    return;
  }
  push(encoded);
}

/**
 * One encoded `ExecuteStreamFrame` message.
 *
 * @param value - The frame.
 * @returns Its single-segment Cap'n Proto bytes.
 */
function frame(value: ExecuteStreamFrame): Uint8Array {
  return encodeMessage(ExecuteStreamFrameCodec, value);
}

/**
 * Rough wire size of one row: cell payloads plus pointer and tag words.
 *
 * @param row - The row.
 * @returns Estimated bytes.
 */
function rowSizeEstimate(row: DbRow): number {
  let size = 16;
  for (const value of row.values) {
    if (value.kind === "text") {
      size += new TextEncoder().encode(value.value).byteLength + 24;
    } else if (value.kind === "bytes") {
      size += value.value.byteLength + 24;
    } else {
      size += 24;
    }
  }
  return size;
}

/**
 * The `request_hash` of a prior-receipt row; empty text still counts as a
 * receipt, as it does in the host.
 *
 * @param prior - The prior-receipt `SELECT` result.
 * @returns The hash, or `undefined` when there is no row or the cell is not text.
 */
function receiptHash(prior: StatementResult): string | undefined {
  const cell = receiptCell(prior, "request_hash", 1);
  return cell?.kind === "text" ? cell.value : undefined;
}

/**
 * A text cell of a prior-receipt row, found by column name or, when no
 * column has that name, by position.
 *
 * @param prior - The prior-receipt `SELECT` result.
 * @param column - Column name (`request_hash`, `status`, `payload`).
 * @param fallback - Position used when no column has that name.
 * @returns The non-empty text, or `undefined` when there is no row or the
 *   cell is not non-empty text.
 */
function receiptText(prior: StatementResult, column: string, fallback: number): string | undefined {
  const cell = receiptCell(prior, column, fallback);
  return cell?.kind === "text" && cell.value !== "" ? cell.value : undefined;
}

/**
 * One cell of a prior-receipt row, by column name or else by position.
 *
 * @param prior - The prior-receipt `SELECT` result.
 * @param column - Column name.
 * @param fallback - Position used when no column has that name.
 * @returns The cell, or `undefined` when there is no row or no such cell.
 */
function receiptCell(
  prior: StatementResult,
  column: string,
  fallback: number,
): DbValue | undefined {
  const row = prior.rows[0];
  if (!row) {
    return undefined;
  }
  const named = prior.columns.findIndex((c) => c.name.toLowerCase() === column);
  return row.values[named >= 0 ? named : fallback];
}

/**
 * A result that changed `n` rows and returned none.
 *
 * @param n - Rows affected.
 * @returns The result.
 */
function affected(n: number): StatementResult {
  return { rows: [], columns: [], rowsAffected: n };
}

/**
 * One statement result in serde form.
 *
 * @param stmt - The result.
 * @returns Its JSON text.
 */
function statementJson(stmt: StatementResult): string {
  const rows = stmt.rows.map((row) => `{"values":[${row.values.map(valueJson).join(",")}]}`);
  const columns = stmt.columns.map(columnJson);
  return `{"rows":[${rows.join(",")}],"columns":[${columns.join(",")}],"rowsAffected":${uint(stmt.rowsAffected)}}`;
}

/**
 * One column descriptor in serde form.
 *
 * @param column - The column.
 * @returns Its JSON text.
 */
function columnJson(column: DbColumn): string {
  return `{"name":${JSON.stringify(column.name)},"dbType":${JSON.stringify(column.dbType)}}`;
}

/**
 * One cell in serde form (`{"kind", "value"}`, bytes as a number array).
 *
 * @param value - The cell.
 * @returns Its JSON text.
 * @throws `PluginError` `internal` for a non-finite float, which JSON cannot carry.
 */
function valueJson(value: DbValue): string {
  switch (value.kind) {
    case "null":
      return `{"kind":"null","value":${JSON.stringify(value.value)}}`;
    case "boolean":
      return `{"kind":"boolean","value":${value.value ? "true" : "false"}}`;
    case "int64":
      return `{"kind":"int64","value":${BigInt(value.value).toString()}}`;
    case "float64":
      if (!Number.isFinite(value.value)) {
        throw PluginError.fromWire("internal", "guest replay payload has a non-finite float");
      }
      // `JSON.stringify(-0)` is `0`; the host writes (and replays) `-0.0`.
      return `{"kind":"float64","value":${Object.is(value.value, -0) ? "-0.0" : JSON.stringify(value.value)}}`;
    case "text":
      return `{"kind":"text","value":${JSON.stringify(value.value)}}`;
    case "bytes":
      return `{"kind":"bytes","value":[${Array.from(value.value).join(",")}]}`;
  }
}

/**
 * A non-negative integer counter as JSON.
 *
 * @param n - The counter.
 * @returns Its decimal text.
 * @throws `PluginError` `internal` when `n` is not a non-negative safe integer.
 */
function uint(n: number): string {
  if (!Number.isSafeInteger(n) || n < 0) {
    throw PluginError.fromWire("internal", `guest replay payload counter ${n} is not a u64`);
  }
  return String(n);
}
