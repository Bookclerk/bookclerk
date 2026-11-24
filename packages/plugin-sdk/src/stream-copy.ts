/**
 * The `stream_copy` job: copy one object from the job's input to its output
 * through streams, staged under a retry-stable commit token and then
 * committed.
 *
 * Storage guests run this job when the host asks a destination to take an
 * object from another store. It matches the Rust SDK's `StreamCopyHandler`
 * and the Python SDK's `stream_copy` byte for byte: the same commit token,
 * the same stage, commit and abort order, and the same outcomes, so a
 * destination behaves the same in every language and runtime.
 */

import { PluginError } from "./errors.js";
import type { JobCompletion, JobController, WriteOptions } from "./plugin.js";

/** The job command type this helper runs. */
export const STREAM_COPY_COMMAND = "stream_copy";

/** JSON payload of a `stream_copy` job. */
export interface StreamCopySpec {
  /** Source object key, read from `controller.input`. */
  from: string;
  /** Destination object key, written to `controller.output`. */
  to: string;
}

/** Lower-case hex digits. */
const HEX_DIGITS = "0123456789abcdef";

/**
 * The retry-stable commit token for one stream-copy step.
 *
 * The token is `sc-` and 32 hex digits of SHA-256 over the idempotency key,
 * a zero byte, the step id (`stream_copy` when empty), a zero byte, and the
 * destination key. It leaves out the invocation id, attempt and generation:
 * publishing is at-least-once, so a reclaimed attempt must stage and commit
 * the same token, and a destination treats an already-published object
 * with a missing stage as committed.
 *
 * @param idempotencyKey - The job's durable idempotency key.
 * @param stepId - The step id; empty for a single-step job.
 * @param destKey - The destination object key.
 * @returns The token (path-safe: letters, digits and `-` only).
 */
export async function streamCopyCommitToken(
  idempotencyKey: string,
  stepId: string,
  destKey: string,
): Promise<string> {
  const encoder = new TextEncoder();
  const parts = [
    encoder.encode(idempotencyKey),
    encoder.encode(stepId || STREAM_COPY_COMMAND),
    encoder.encode(destKey),
  ];
  const total = parts.reduce((sum, part) => sum + part.length, 0) + 2;
  const input = new Uint8Array(total);
  let at = 0;
  parts.forEach((part, index) => {
    if (index > 0) at += 1; // the zero separator
    input.set(part, at);
    at += part.length;
  });
  const digest = new Uint8Array(await globalThis.crypto.subtle.digest("SHA-256", input));
  let hex = "";
  for (const byte of digest.subarray(0, 16)) {
    hex += HEX_DIGITS.charAt(byte >> 4) + HEX_DIGITS.charAt(byte & 15);
  }
  return `sc-${hex}`;
}

/**
 * Reads the job payload as a {@link StreamCopySpec}.
 *
 * @param payloadJson - The payload text.
 * @returns The spec.
 * @throws {PluginError} `invalid_params` when it is not an object with
 *   string `from` and `to`.
 */
function parseSpec(payloadJson: string): StreamCopySpec {
  let value: unknown;
  try {
    value = JSON.parse(payloadJson) as unknown;
  } catch (err) {
    throw PluginError.fromWire("invalid_params", `stream_copy spec: ${(err as Error).message}`);
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw PluginError.fromWire("invalid_params", "stream_copy spec: expected an object");
  }
  const { from, to } = value as Record<string, unknown>;
  if (typeof from !== "string" || typeof to !== "string") {
    throw PluginError.fromWire("invalid_params", "stream_copy spec: `from` and `to` are strings");
  }
  return { from, to };
}

/**
 * Runs one `stream_copy` job: open `from` on the input, stage it on the
 * output under {@link streamCopyCommitToken}, check for cancellation, then
 * commit. Cancellation, or a failed put, progress report or commit, aborts
 * the stage (best effort) before the error goes back.
 *
 * Call it from the default entrypoint's `job(controller)`:
 *
 * ```ts
 * async job(controller: JobController) {
 *   return streamCopy(controller);
 * }
 * ```
 *
 * @param controller - The job invocation.
 * @returns The completion, carrying the bytes written.
 * @throws {PluginError} `unsupported` for another command type,
 *   `invalid_params` for a bad payload, `unavailable` when the host granted
 *   no input or output, `cancelled` when the host cancelled the job, or the
 *   input's or output's own error.
 */
export async function streamCopy(controller: JobController): Promise<JobCompletion> {
  if (controller.signal.aborted) {
    throw PluginError.fromWire("cancelled", "cancelled before stream_copy");
  }
  if (controller.type !== STREAM_COPY_COMMAND) {
    throw PluginError.fromWire("unsupported", `unsupported job command \`${controller.type}\``);
  }
  const spec = parseSpec(controller.payloadJson);
  const input = controller.input;
  const output = controller.output;
  if (input === null || output === null) {
    throw PluginError.fromWire("unavailable", "stream_copy needs the job's input and output");
  }
  const token = await streamCopyCommitToken(controller.idempotencyKey, controller.stepId, spec.to);
  const abort = async (): Promise<void> => {
    try {
      await output.abortStage(spec.to, token);
    } catch {
      // Best effort: the error the caller sees is the one that stopped the copy.
    }
  };
  await controller.progress(0, "opening source");
  const read = await input.open(spec.from);
  const options: WriteOptions = { commitToken: token, stageOnly: true };
  if (read.meta.contentType !== undefined) options.contentType = read.meta.contentType;
  if (read.meta.size > 0) options.contentLength = read.meta.size;
  if (read.meta.sha256 !== undefined && read.meta.sha256.length > 0)
    options.sha256 = read.meta.sha256;
  await controller.progress(10, "staging");
  let bytesWritten: number;
  try {
    bytesWritten = (await output.put(spec.to, read.body, options)).bytesWritten;
  } catch (err) {
    await abort();
    throw err;
  }
  if (controller.signal.aborted) {
    await abort();
    throw PluginError.fromWire("cancelled", "cancelled after stage");
  }
  try {
    await controller.progress(90, "committing");
    await output.commit(spec.to, token);
  } catch (err) {
    await abort();
    throw err;
  }
  await controller.progress(100, "done");
  return { message: `copied ${spec.from} -> ${spec.to}`, bytesCopied: bytesWritten };
}
