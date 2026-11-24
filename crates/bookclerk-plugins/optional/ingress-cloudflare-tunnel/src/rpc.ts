/**
 * The tunnel's control channel: Cap'n Proto RPC messages (`rpc.capnp`)
 * carrying `RegistrationServer.registerConnection` (cloudflared's
 * `tunnelrpc.capnp`, Apache-2.0).
 *
 * The connector asks the edge for its bootstrap capability (question 0),
 * pipelines `registerConnection` on that promise (question 1) and reads the
 * `Return`. The edge has no reason to call the connector, so any call or
 * bootstrap request it sends is answered with an `unimplemented`
 * exception. Struct layouts are those the Cap'n Proto compiler assigns to
 * the two schemas; the tests check the bytes against messages the
 * reference encoder produced.
 */

import { Builder, CapnpError, readRoot, type StructReader } from "./capnp.ts";

/** `RegistrationServer`'s interface id. */
export const REGISTRATION_SERVER = 0xf71695ec7fe85497n;

/** `registerConnection`'s method ordinal. */
export const REGISTER_CONNECTION = 0;

/** The question id of the bootstrap request. */
export const BOOTSTRAP_QUESTION = 0;

/** The question id of the `registerConnection` call. */
export const REGISTER_QUESTION = 1;

/** `Message` union tags. */
const Tag = {
  Unimplemented: 0,
  Abort: 1,
  Call: 2,
  Return: 3,
  Finish: 4,
  Bootstrap: 8,
} as const;

/** `Return` union tags. */
const ReturnTag = { Results: 0, Exception: 1 } as const;

/** `Exception.Type.unimplemented`. */
const EXCEPTION_UNIMPLEMENTED = 3;

/** What the connector sends to register. */
export interface Registration {
  /** The account the tunnel belongs to. */
  accountTag: string;
  /** The tunnel's secret. */
  tunnelSecret: Uint8Array;
  /** The tunnel's id, 16 bytes. */
  tunnelId: Uint8Array;
  /** This connection's index among the connector's connections. */
  connIndex: number;
  /** A stable id for this connector, 16 bytes. */
  clientId: Uint8Array;
  /** The connector's version string. */
  version: string;
  /** Its platform. */
  arch: string;
  /** Whether to replace a connection the edge still holds for this index. */
  replaceExisting: boolean;
  /** How many registrations failed before this one. */
  previousAttempts: number;
}

/**
 * Wraps a body struct in a `Message`.
 *
 * @param tag - The union tag.
 * @param dataWords - The body's data words.
 * @param ptrs - The body's pointers.
 * @returns The builder and the body.
 */
function message(
  tag: number,
  dataWords: number,
  ptrs: number,
): { b: Builder; body: ReturnType<Builder["root"]> } {
  const b = new Builder();
  const root = b.root(1, 1).uint(0, 2, tag);
  return { b, body: root.struct(0, dataWords, ptrs) };
}

/**
 * The bootstrap request.
 *
 * @returns The framed message.
 */
export function encodeBootstrap(): Uint8Array {
  const { b, body } = message(Tag.Bootstrap, 1, 1);
  body.uint(0, 4, BOOTSTRAP_QUESTION);
  return b.frame();
}

/**
 * The `registerConnection` call, pipelined on the bootstrap answer.
 *
 * @param reg - What to register.
 * @returns The framed message.
 */
export function encodeRegisterConnection(reg: Registration): Uint8Array {
  const { b, body: call } = message(Tag.Call, 3, 3);
  call
    .uint(0, 4, REGISTER_QUESTION)
    .uint(4, 2, REGISTER_CONNECTION)
    .uint(6, 2, 0) // sendResultsTo: caller
    .uint64(8, REGISTRATION_SERVER);
  // target: promisedAnswer { questionId: 0, transform: [] }.
  const target = call.struct(0, 1, 1).uint(4, 2, 1);
  target.struct(0, 1, 1).uint(0, 4, BOOTSTRAP_QUESTION);
  // params: Payload { content: RegisterConnectionParams }.
  const params = call.struct(1, 0, 2).struct(0, 1, 3);
  params.struct(0, 0, 2).text(0, reg.accountTag).data(1, reg.tunnelSecret);
  params.data(1, reg.tunnelId).uint(0, 1, reg.connIndex);
  const options = params
    .struct(2, 1, 2)
    .bool(0, reg.replaceExisting)
    .uint(1, 1, 0)
    .uint(2, 1, reg.previousAttempts);
  options
    .struct(0, 0, 4)
    .data(0, reg.clientId)
    .textList(1, [])
    .text(2, reg.version)
    .text(3, reg.arch);
  return b.frame();
}

/**
 * A `Finish` for a question whose answer arrived.
 *
 * @param question - The question id.
 * @returns The framed message.
 */
export function encodeFinish(question: number): Uint8Array {
  const { b, body } = message(Tag.Finish, 1, 0);
  body.uint(0, 4, question);
  return b.frame();
}

/**
 * A `Return` carrying an `unimplemented` exception, for a call or bootstrap
 * request the connector does not serve.
 *
 * @param answer - The question id being answered.
 * @param reason - Why.
 * @returns The framed message.
 */
export function encodeUnimplementedReturn(answer: number, reason: string): Uint8Array {
  const { b, body } = message(Tag.Return, 2, 1);
  body.uint(0, 4, answer).uint(6, 2, ReturnTag.Exception);
  body.struct(0, 1, 3).text(0, reason).uint(4, 2, EXCEPTION_UNIMPLEMENTED);
  return b.frame();
}

/** What the edge answered `registerConnection` with. */
export type Registered =
  /** The connection serves. */
  | { kind: "registered"; uuid: Uint8Array; location: string; remotelyManaged: boolean }
  /** The edge refused it. */
  | { kind: "refused"; cause: string; retryAfterMs: number; shouldRetry: boolean };

/** A message from the edge, as the connector acts on it. */
export type Incoming =
  /** The answer to one of the connector's questions. */
  | {
      kind: "return";
      answer: number;
      result: Registered | { kind: "exception"; reason: string } | null;
    }
  /** A question the connector must answer (bootstrap or call). */
  | { kind: "question"; question: number }
  /** The edge ended the channel. */
  | { kind: "abort"; reason: string }
  /** The edge did not understand a message the connector sent. */
  | { kind: "unimplemented" }
  /** Anything else (Finish, Release and the rest), which needs nothing. */
  | { kind: "other" };

/**
 * Reads a `ConnectionResponse`.
 *
 * @param response - The struct.
 * @returns What it says.
 * @throws {CapnpError} When a field is malformed.
 */
function connectionResponse(response: StructReader): Registered {
  const tag = response.uint(0, 2);
  const inner = response.struct(0);
  if (tag === 1) {
    return {
      kind: "registered",
      uuid: inner?.data(0) ?? new Uint8Array(),
      location: inner?.text(1) ?? "",
      remotelyManaged: inner?.bool(0) ?? false,
    };
  }
  if (tag !== 0) throw new CapnpError(`ConnectionResponse tag ${tag}`);
  const retryNs = inner?.int64(0) ?? 0n;
  return {
    kind: "refused",
    cause: inner?.text(0) ?? "",
    retryAfterMs: Number(retryNs > 0n ? retryNs / 1_000_000n : 0n),
    shouldRetry: inner?.bool(64) ?? false,
  };
}

/**
 * Reads one message from the edge.
 *
 * @param segments - The message's segments.
 * @returns What it means for the connector.
 * @throws {CapnpError} When it is malformed.
 */
export function readIncoming(segments: Uint8Array[]): Incoming {
  const root = readRoot(segments);
  const tag = root.uint(0, 2);
  switch (tag) {
    case Tag.Return: {
      const ret = root.struct(0);
      if (ret === null) throw new CapnpError("Return without a body");
      const answer = ret.uint(0, 4);
      const which = ret.uint(6, 2);
      if (which === ReturnTag.Exception) {
        return {
          kind: "return",
          answer,
          result: { kind: "exception", reason: ret.struct(0)?.text(0) ?? "" },
        };
      }
      if (which !== ReturnTag.Results || answer !== REGISTER_QUESTION) {
        return { kind: "return", answer, result: null };
      }
      const content = ret.struct(0)?.struct(0) ?? null;
      const response = content?.struct(0) ?? null;
      if (response === null) throw new CapnpError("registerConnection returned nothing");
      return { kind: "return", answer, result: connectionResponse(response) };
    }
    case Tag.Bootstrap:
    case Tag.Call: {
      const body = root.struct(0);
      return { kind: "question", question: body?.uint(0, 4) ?? 0 };
    }
    case Tag.Abort:
      return { kind: "abort", reason: root.struct(0)?.text(0) ?? "" };
    case Tag.Unimplemented:
      return { kind: "unimplemented" };
    default:
      return { kind: "other" };
  }
}
