/**
 * Streamed execute replies (`GuestDatabase.executeStreamed`).
 *
 * An adapter answers a streamed batch as consecutive single-segment Cap'n
 * Proto `ExecuteStreamFrame` messages: `head`, then per statement a
 * `statement` frame and its `rows` frames, then `end` (or `err` at any
 * point). {@link ExecuteStreamDecoder} reassembles them under the session's
 * streamed budget, failing closed with a typed `limit_exceeded` as soon as
 * the budget is passed, so an oversized result never lands whole. The same
 * decoder, fed in any split, yields the same reply as one push.
 */

import { MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES } from "./abi.js";
import type { ExecuteReply, StatementResult } from "./db-execute.js";
import { PluginError } from "./errors.js";
import type { DbCapabilities, DbColumn, DbValue, ExecuteStreamFrame } from "./generated.js";
import { CapnpReader, NO_CAPS } from "./db-capnp.js";
import { ExecuteStreamFrameCodec } from "./generated-wire.js";

/**
 * What a receiver lets a streamed reply grow to.
 */
export interface ExecuteStreamLimits {
  /** Encoded reply budget in bytes (`0` = only the framing slack). */
  maxBytes: number;
  /** Capability field `maxBytes` came from, for the error it raises. */
  bytesLimit: "maxStreamedResultBytes" | "maxAtomicResultBytes";
  /** Rows one statement may return (`0` = unlimited). */
  maxRows: number;
  /** Capability field `maxRows` came from, for the error it raises. */
  rowsLimit: "maxStreamedResultRows" | "maxResultRows";
}

/**
 * True when `caps` advertises a streamed result budget.
 *
 * @param caps - Session capabilities (`limits()`).
 * @returns Whether `executeStreamed` is worth calling on this session.
 */
export function streamsResults(caps: DbCapabilities): boolean {
  return caps.maxStreamedResultBytes > 0 && caps.maxStreamedResultRows > 0;
}

/**
 * The streamed budget of `caps`, or its inline caps when the session does
 * not stream.
 *
 * @param caps - Session capabilities (`limits()`).
 * @returns Limits for an {@link ExecuteStreamDecoder}.
 */
export function executeStreamLimits(caps: DbCapabilities): ExecuteStreamLimits {
  if (streamsResults(caps)) {
    return {
      maxBytes: caps.maxStreamedResultBytes,
      bytesLimit: "maxStreamedResultBytes",
      maxRows: caps.maxStreamedResultRows,
      rowsLimit: "maxStreamedResultRows",
    };
  }
  return {
    maxBytes: caps.maxAtomicResultBytes,
    bytesLimit: "maxAtomicResultBytes",
    maxRows: caps.maxResultRows,
    rowsLimit: "maxResultRows",
  };
}

/**
 * Raw frame bytes a receiver accepts before failing closed: the reply budget
 * plus one eighth for frame headers and one window of slack.
 *
 * @param limits - Budget.
 * @returns Byte ceiling on the whole frame sequence and on any one frame.
 */
export function rawStreamCeiling(limits: ExecuteStreamLimits): number {
  return limits.maxBytes + Math.floor(limits.maxBytes / 8) + MAX_STREAM_WINDOW_BYTES;
}

/** Decoder position in the frame sequence. */
type DecoderState =
  | { kind: "awaitingHead" }
  | {
      kind: "inReply";
      operationId: string;
      statementCount: number;
      nextIndex: number;
      statements: StatementResult[];
      open: StatementResult | undefined;
      /** Rows the open statement has carried, kept or not. */
      openRows: number;
    }
  | { kind: "finished"; outcome: { ok: ExecuteReply } | { err: PluginError } };

/**
 * Sans-IO decoder of a streamed execute reply: feed it bytes in any split,
 * then take the reassembled {@link ExecuteReply}.
 *
 * It fails closed on more raw bytes than the budget allows, on a statement
 * with more rows than the row cap, and on any frame out of sequence.
 */
export class ExecuteStreamDecoder {
  readonly #limits: ExecuteStreamLimits;
  readonly #retainRows: boolean;
  #buf: Uint8Array = new Uint8Array(0);
  #seen = 0;
  #state: DecoderState = { kind: "awaitingHead" };
  #columns: DbColumn[] = [];

  /**
   * @param limits - Budget the stream is held to.
   * @param options - `retainRows: false` counts rows against the budget but
   *   drops them from the collected reply (for `iterate()`).
   */
  constructor(limits: ExecuteStreamLimits, options: { retainRows?: boolean } = {}) {
    this.#limits = limits;
    this.#retainRows = options.retainRows ?? true;
  }

  /**
   * Whether the final frame (`end` or `err`) has been decoded.
   *
   * @returns True once the stream is complete.
   */
  get finished(): boolean {
    return this.#state.kind === "finished";
  }

  /**
   * Columns of the statement whose rows are arriving.
   *
   * @returns The open statement's columns, or an empty list between statements.
   */
  get columns(): DbColumn[] {
    return this.#columns;
  }

  /**
   * Feeds the next bytes of the stream.
   *
   * @param bytes - Any slice of the frame sequence.
   * @returns The frames completed by these bytes, in order.
   * @throws `PluginError` `limit_exceeded` when the stream exceeds the byte
   *   or row budget, `invalid_params` when a frame is malformed or out of
   *   sequence or bytes follow the final frame.
   */
  push(bytes: Uint8Array): ExecuteStreamFrame[] {
    try {
      return this.#push(bytes);
    } catch (err) {
      // A refused stream stays refused: `finish()` after a failed push
      // reports the failure, never a reply the earlier frames described.
      if (err instanceof PluginError) {
        this.#columns = [];
        this.#state = { kind: "finished", outcome: { err } };
      }
      throw err;
    }
  }

  /**
   * `push` before its failure is latched.
   *
   * @param bytes - Any slice of the frame sequence.
   * @returns The frames completed by these bytes, in order.
   * @throws See {@link ExecuteStreamDecoder.push}.
   */
  #push(bytes: Uint8Array): ExecuteStreamFrame[] {
    const frames: ExecuteStreamFrame[] = [];
    if (bytes.byteLength === 0) {
      return frames;
    }
    if (this.finished) {
      throw PluginError.fromWire(
        "invalid_params",
        "streamed reply carries bytes after its final frame",
      );
    }
    this.#seen += bytes.byteLength;
    const ceiling = rawStreamCeiling(this.#limits);
    if (this.#seen > ceiling) {
      throw this.#bytesExceeded(this.#seen);
    }
    this.#buf = concat(this.#buf, bytes);
    // Completed frames are consumed by offset; only an incomplete remainder
    // is carried (as a view) into the next push, so a reply of many frames
    // is decoded in one pass over its bytes.
    let at = 0;
    while (this.#buf.byteLength - at >= 8) {
      const view = new DataView(this.#buf.buffer, this.#buf.byteOffset + at, 8);
      if (view.getUint32(0, true) !== 0) {
        throw PluginError.fromWire(
          "invalid_params",
          "streamed reply frame is not a single-segment Cap'n message",
        );
      }
      const frameLen = view.getUint32(4, true) * 8 + 8;
      if (frameLen > ceiling) {
        throw this.#bytesExceeded(frameLen);
      }
      if (this.#buf.byteLength - at < frameLen) {
        break;
      }
      // Each frame is decoded from its own copy, so a BYTES cell an author
      // keeps does not pin the rest of the stream in memory.
      const frame = decodeFrame(this.#buf.slice(at, at + frameLen), ceiling);
      at += frameLen;
      this.#apply(frame);
      frames.push(frame);
      if (this.finished) {
        if (this.#buf.byteLength > at) {
          throw PluginError.fromWire(
            "invalid_params",
            "streamed reply carries bytes after its final frame",
          );
        }
        break;
      }
    }
    this.#buf = at === 0 ? this.#buf : this.#buf.subarray(at);
    return frames;
  }

  /**
   * Takes the reassembled reply once the stream has ended.
   *
   * @returns The reply the frames described.
   * @throws The adapter's `PluginError` when the stream ended with `err`, and
   *   `unavailable` when it ended before its final frame.
   */
  finish(): ExecuteReply {
    if (this.#state.kind !== "finished") {
      throw PluginError.fromWire("unavailable", "streamed reply ended before its final frame");
    }
    const outcome = this.#state.outcome;
    if ("err" in outcome) {
      throw outcome.err;
    }
    return outcome.ok;
  }

  /**
   * The typed error for a stream over its byte budget.
   *
   * @param used - Bytes seen or declared.
   * @returns `limit_exceeded` naming the budget's capability field.
   */
  #bytesExceeded(used: number): PluginError {
    return PluginError.limitExceeded({
      limit: this.#limits.bytesLimit,
      cap: this.#limits.maxBytes,
      used,
    });
  }

  /**
   * The error for a frame out of sequence.
   *
   * @param message - What the stream did, completing "streamed reply …".
   * @returns `invalid_params`.
   */
  #sequence(message: string): PluginError {
    return PluginError.fromWire("invalid_params", `streamed reply ${message}`);
  }

  /**
   * Advances the sequence by one decoded frame: opens the reply on `head`,
   * a statement on `statement`, counts (and keeps, when retained) `rows`,
   * and closes the reply on `end` or `err`.
   *
   * @param frame - The frame just decoded.
   * @throws `PluginError` `invalid_params` when the frame is out of sequence
   *   or a cell is not a valid `DbValue`, `limit_exceeded` past the row cap.
   */
  #apply(frame: ExecuteStreamFrame): void {
    const state = this.#state;
    if (frame.kind === "err") {
      this.#columns = [];
      this.#state = {
        kind: "finished",
        outcome: {
          err: PluginError.fromWire(frame.value.code, frame.value.message, frame.value.details),
        },
      };
      return;
    }
    if (state.kind === "awaitingHead") {
      if (frame.kind !== "head") {
        throw this.#sequence(`opens with a ${frame.kind} frame instead of head`);
      }
      this.#state = {
        kind: "inReply",
        operationId: frame.value.operationId,
        statementCount: frame.value.statementCount,
        nextIndex: 0,
        statements: [],
        open: undefined,
        openRows: 0,
      };
      return;
    }
    if (state.kind === "finished") {
      throw this.#sequence("carries a frame after its final frame");
    }
    switch (frame.kind) {
      case "head":
        throw this.#sequence("carries a second head frame");
      case "statement": {
        if (frame.value.index !== state.nextIndex) {
          throw this.#sequence(
            `statement frame ${frame.value.index} arrived where ${state.nextIndex} was expected`,
          );
        }
        if (state.nextIndex >= state.statementCount) {
          throw this.#sequence(`carries more statements than its head announced`);
        }
        if (state.open) {
          state.statements.push(state.open);
        }
        state.open = {
          rows: [],
          columns: frame.value.columns,
          rowsAffected: frame.value.rowsAffected,
        };
        state.openRows = 0;
        state.nextIndex += 1;
        this.#columns = frame.value.columns;
        return;
      }
      case "rows": {
        const open = state.open;
        if (!open) {
          throw this.#sequence("carries rows before a statement frame");
        }
        for (const row of frame.value) {
          for (const cell of row.values) {
            validateCell(cell);
          }
        }
        state.openRows += frame.value.length;
        if (this.#limits.maxRows > 0 && state.openRows > this.#limits.maxRows) {
          throw PluginError.limitExceeded({
            limit: this.#limits.rowsLimit,
            cap: this.#limits.maxRows,
            used: state.openRows,
          });
        }
        if (this.#retainRows) {
          for (const row of frame.value) {
            open.rows.push(row);
          }
        }
        return;
      }
      case "end": {
        if (state.open) {
          state.statements.push(state.open);
          state.open = undefined;
        }
        if (state.statements.length !== state.statementCount) {
          throw this.#sequence(
            `ended with ${state.statements.length} statements; its head announced ${state.statementCount}`,
          );
        }
        this.#columns = [];
        this.#state = {
          kind: "finished",
          outcome: {
            ok: {
              operationId: state.operationId,
              statements: state.statements,
              timing: frame.value,
            },
          },
        };
        return;
      }
    }
  }
}

/**
 * Decodes a whole frame sequence under `limits`.
 *
 * @param bytes - Every frame, concatenated.
 * @param limits - Budget the stream is held to.
 * @returns The reassembled reply.
 * @throws See {@link ExecuteStreamDecoder.push} and {@link ExecuteStreamDecoder.finish}.
 */
export function decodeExecuteStream(bytes: Uint8Array, limits: ExecuteStreamLimits): ExecuteReply {
  const decoder = new ExecuteStreamDecoder(limits);
  decoder.push(bytes);
  return decoder.finish();
}

/**
 * Collects an async chunk source into one reply under `limits`.
 *
 * @param chunks - Frame bytes as the transport delivers them.
 * @param limits - Budget the stream is held to.
 * @returns The reassembled reply.
 */
export async function collectExecuteStream(
  chunks: AsyncIterable<Uint8Array>,
  limits: ExecuteStreamLimits,
): Promise<ExecuteReply> {
  const decoder = new ExecuteStreamDecoder(limits);
  for await (const chunk of chunks) {
    decoder.push(chunk);
  }
  return decoder.finish();
}

/**
 * Reads a web `ReadableStream` body as async chunks.
 *
 * @param body - Response body, or `null` for an empty one.
 * @yields The body's chunks in order.
 */
export async function* streamChunks(
  body: ReadableStream<Uint8Array> | null,
): AsyncIterable<Uint8Array> {
  if (!body) {
    return;
  }
  const reader = body.getReader();
  let drained = false;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        drained = true;
        return;
      }
      if (value) {
        yield value;
      }
    }
  } finally {
    // A consumer that stops early (an `iterate()` loop that breaks, a
    // decoder that refused a frame) releases the unread body instead of
    // leaving the transfer parked behind the lock.
    if (!drained) {
      await reader.cancel().catch(() => undefined);
    }
    reader.releaseLock();
  }
}

/**
 * Checks a streamed cell the way the inline reader does: a `float64` must
 * be finite and TEXT must not contain U+0000.
 *
 * @param cell - A decoded cell.
 * @throws `PluginError` `invalid_params` when the cell is not a valid `DbValue`.
 */
function validateCell(cell: DbValue): void {
  if (cell.kind === "float64" && !Number.isFinite(cell.value)) {
    throw PluginError.fromWire("invalid_params", "streamed reply float64 cell is not finite");
  }
  if (cell.kind === "text" && cell.value.includes("\u0000")) {
    throw PluginError.fromWire("invalid_params", "streamed reply TEXT cell contains U+0000");
  }
}

/**
 * Decodes one frame under the traversal budget its stream negotiated.
 *
 * A frame carrying one wide row can be larger than the budget that suits
 * control messages, so the reader's budget follows the frame's own size,
 * clamped to the stream's raw ceiling, as the host's decoder does.
 *
 * @param bytes - One complete single-segment frame.
 * @param ceiling - Raw byte ceiling of the stream (`rawStreamCeiling`).
 * @returns The decoded frame.
 * @throws `PluginError` `invalid_params` when the frame does not decode.
 */
function decodeFrame(bytes: Uint8Array, ceiling: number): ExecuteStreamFrame {
  const words = Math.min(
    Math.floor(bytes.byteLength / 8) + 16,
    Math.floor(Math.max(ceiling, MAX_SCALAR_BYTES) / 8),
  );
  try {
    const reader = new CapnpReader(bytes, words);
    return ExecuteStreamFrameCodec.read(
      reader.root(ExecuteStreamFrameCodec.dataWords, ExecuteStreamFrameCodec.pointerCount),
      NO_CAPS,
    );
  } catch (err) {
    throw PluginError.fromWire(
      "invalid_params",
      `streamed reply frame does not decode: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

/**
 * Joins two byte runs, reusing `b` when `a` is empty.
 *
 * @param a - Bytes carried over from the previous push.
 * @param b - Bytes just pushed.
 * @returns `a` followed by `b`.
 */
function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.byteLength === 0) {
    return b;
  }
  const out = new Uint8Array(a.byteLength + b.byteLength);
  out.set(a, 0);
  out.set(b, a.byteLength);
  return out;
}
