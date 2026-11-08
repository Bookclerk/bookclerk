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

import { MAX_STREAM_WINDOW_BYTES } from "./abi.js";
import type { ExecuteReply, StatementResult } from "./db-execute.js";
import { PluginError } from "./errors.js";
import type { DbCapabilities, DbColumn, ExecuteStreamFrame } from "./generated.js";
import { ExecuteStreamFrameCodec, decodeMessage } from "./generated-wire.js";

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
    while (this.#buf.byteLength >= 8) {
      const view = new DataView(this.#buf.buffer, this.#buf.byteOffset, this.#buf.byteLength);
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
      if (this.#buf.byteLength < frameLen) {
        break;
      }
      let frame: ExecuteStreamFrame;
      try {
        frame = decodeMessage(ExecuteStreamFrameCodec, this.#buf.subarray(0, frameLen));
      } catch (err) {
        throw PluginError.fromWire(
          "invalid_params",
          `streamed reply frame does not decode: ${err instanceof Error ? err.message : String(err)}`,
        );
      }
      this.#buf = this.#buf.slice(frameLen);
      this.#apply(frame);
      frames.push(frame);
      if (this.finished) {
        if (this.#buf.byteLength > 0) {
          throw PluginError.fromWire(
            "invalid_params",
            "streamed reply carries bytes after its final frame",
          );
        }
        break;
      }
    }
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

  #bytesExceeded(used: number): PluginError {
    return PluginError.limitExceeded({
      limit: this.#limits.bytesLimit,
      cap: this.#limits.maxBytes,
      used,
    });
  }

  #sequence(message: string): PluginError {
    return PluginError.fromWire("invalid_params", `streamed reply ${message}`);
  }

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
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        return;
      }
      if (value) {
        yield value;
      }
    }
  } finally {
    reader.releaseLock();
  }
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.byteLength === 0) {
    return b;
  }
  const out = new Uint8Array(a.byteLength + b.byteLength);
  out.set(a, 0);
  out.set(b, a.byteLength);
  return out;
}
