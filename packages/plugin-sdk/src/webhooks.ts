/**
 * Author-side `WEBHOOKS` binding: the instance's inbound webhooks, read like
 * a pulled Workers Queue.
 *
 * The adapter isolate holds the webhooks-only grant and exposes one
 * {@link WebhooksTransport} call per `POST /webhooks/<op>`; this module turns
 * it into the {@link WebhooksBinding} on the author's `env`. Transport replies
 * carry failures as values, not throws, so a `PluginError` keeps its code
 * across the Workers RPC boundary between the two isolates.
 */

import {
  MAX_WEBHOOK_LEASE_ID_BYTES,
  MAX_WEBHOOK_NAME_BYTES,
  MAX_WEBHOOK_PULL_BATCH,
  MAX_WEBHOOK_VISIBILITY_MS,
} from "./abi.js";
import type {
  WebhookMessage,
  WebhookPullOptions,
  WebhookPullResult,
  WebhooksBinding,
} from "./env.js";
import { PluginError } from "./errors.js";

/** Operations of the `/webhooks/<op>` route. */
export type WebhookOp = "url" | "pull" | "ack";

/** One transport answer: the route's JSON body, or a typed failure. */
export type WebhookTransportReply =
  { ok: Record<string, unknown> } | { err: { code: string; message: string } };

/**
 * The adapter's webhooks grant, as the author isolate reaches it. Never
 * throws: every failure comes back as `err`.
 */
export interface WebhooksTransport {
  /**
   * Make one `POST /webhooks/<op>` call.
   *
   * @param op - Operation.
   * @param body - That operation's JSON fields.
   * @returns The reply body or a typed failure.
   */
  call(op: WebhookOp, body: Record<string, unknown>): Promise<WebhookTransportReply>;
}

/** Batch size a pull uses when the author passes none. */
export const DEFAULT_WEBHOOK_PULL_BATCH = 10;

/**
 * Checks a webhook name the way the host does: `[a-z0-9][a-z0-9-]*`, at most
 * `MAX_WEBHOOK_NAME_BYTES`.
 *
 * @param name - Name the author passed.
 * @returns The name.
 * @throws PluginError `invalid_params` for a malformed name.
 */
export function checkWebhookName(name: unknown): string {
  const text = String(name ?? "");
  if (
    text.length === 0 ||
    text.length > MAX_WEBHOOK_NAME_BYTES ||
    !/^[a-z0-9][a-z0-9-]*$/.test(text)
  ) {
    throw PluginError.fromWire("invalid_params", "malformed webhook name");
  }
  return text;
}

/**
 * Checks a list of lease ids.
 *
 * @param leases - Ids the author passed.
 * @returns The ids as strings.
 * @throws PluginError `invalid_params` for a malformed id or too many ids.
 */
export function checkLeases(leases: unknown): string[] {
  const list = Array.isArray(leases) ? leases.map(String) : [];
  if (list.length > MAX_WEBHOOK_PULL_BATCH) {
    throw PluginError.fromWire(
      "invalid_params",
      `a webhook ack names at most ${MAX_WEBHOOK_PULL_BATCH} leases in each list`,
    );
  }
  for (const lease of list) {
    if (
      lease.length === 0 ||
      lease.length > MAX_WEBHOOK_LEASE_ID_BYTES ||
      !/^[A-Za-z0-9_-]+$/.test(lease)
    ) {
      throw PluginError.fromWire("invalid_params", "malformed webhook lease id");
    }
  }
  return list;
}

/**
 * Standard base64 to bytes; anything that is not a string decodes as empty.
 *
 * @param text - Base64 text.
 * @returns Decoded bytes.
 */
function fromBase64(text: unknown): Uint8Array {
  if (typeof text !== "string" || text === "") return new Uint8Array();
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/**
 * Makes one call and turns a failure back into a `PluginError`.
 *
 * @param transport - The adapter's grant.
 * @param op - Operation.
 * @param body - Fields.
 * @returns The reply body.
 * @throws PluginError carrying the host's code.
 */
async function call(
  transport: WebhooksTransport,
  op: WebhookOp,
  body: Record<string, unknown>,
): Promise<Record<string, unknown>> {
  const reply = await transport.call(op, body);
  if ("err" in reply) {
    throw PluginError.fromWire(reply.err.code || "internal", reply.err.message || "");
  }
  return reply.ok ?? {};
}

/**
 * The author view of one stored request from the route's JSON.
 *
 * @param wire - One `deliveries` entry.
 * @returns The message.
 */
function messageOf(wire: Record<string, unknown>): WebhookMessage {
  const headers: Record<string, string> = {};
  if (Array.isArray(wire.headers)) {
    for (const header of wire.headers as Array<Record<string, unknown>>) {
      if (header && typeof header.name === "string") {
        headers[header.name] = String(header.value ?? "");
      }
    }
  }
  return Object.freeze({
    leaseId: String(wire.leaseId ?? ""),
    timestamp: new Date(Number(wire.receivedAtUnixMs ?? 0) || 0),
    attempts: Number(wire.attempt ?? 0) || 0,
    headers: Object.freeze(headers),
    body: fromBase64(wire.body),
  });
}

/**
 * The author's `WEBHOOKS` binding over the adapter's grant.
 *
 * @param transport - The adapter's grant.
 * @returns The binding installed on `env`.
 */
export function webhooksBinding(transport: WebhooksTransport): WebhooksBinding {
  return Object.freeze({
    async url(name: string): Promise<string | null> {
      const reply = await call(transport, "url", { name: checkWebhookName(name) });
      return typeof reply.url === "string" && reply.url !== "" ? reply.url : null;
    },
    async pull(name: string, options?: WebhookPullOptions): Promise<WebhookPullResult> {
      const batchSize = Math.floor(Number(options?.batchSize ?? DEFAULT_WEBHOOK_PULL_BATCH));
      if (!(batchSize >= 1 && batchSize <= MAX_WEBHOOK_PULL_BATCH)) {
        throw PluginError.fromWire(
          "invalid_params",
          `webhook batch size must be 1..=${MAX_WEBHOOK_PULL_BATCH}`,
        );
      }
      const visibilityTimeoutMs = Math.floor(Number(options?.visibilityTimeoutMs ?? 0));
      if (!(visibilityTimeoutMs >= 0 && visibilityTimeoutMs <= MAX_WEBHOOK_VISIBILITY_MS)) {
        throw PluginError.fromWire(
          "invalid_params",
          `webhook visibility timeout must be at most ${MAX_WEBHOOK_VISIBILITY_MS} ms`,
        );
      }
      const reply = await call(transport, "pull", {
        name: checkWebhookName(name),
        batchSize,
        visibilityTimeoutMs,
      });
      const deliveries = Array.isArray(reply.deliveries)
        ? (reply.deliveries as Array<Record<string, unknown>>)
        : [];
      return { messages: deliveries.map(messageOf), missed: reply.missed === true };
    },
    async ack(name: string, leases: { acks?: string[]; retries?: string[] }): Promise<void> {
      const acks = checkLeases(leases?.acks);
      const retries = checkLeases(leases?.retries);
      if (retries.some((lease) => acks.includes(lease))) {
        throw PluginError.fromWire(
          "invalid_params",
          "a webhook lease is both acknowledged and retried",
        );
      }
      await call(transport, "ack", { name: checkWebhookName(name), acks, retries });
    },
  });
}
