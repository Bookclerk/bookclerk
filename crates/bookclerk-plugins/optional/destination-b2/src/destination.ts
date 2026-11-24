/**
 * Builds the B2 destination from an instance's settings and secrets.
 *
 * Kept out of `index.ts` because workerd treats every named export of the
 * main module as an entrypoint, so that module exports only entrypoint
 * classes.
 */

import {
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  ObjectStoreDestination,
  PluginError,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { B2Store, type Fetcher } from "./backend.ts";
import { B2SigningSecret } from "./b2-events.ts";
import { B2ChangeFeed } from "./feed.ts";

/** The `[[webhooks]]` row B2's event notifications post to. */
export const WEBHOOK = "changes";

/** What every B2 destination does natively, whatever its settings. */
export const RPC_FEATURES: string[] = [
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  ...ObjectStoreDestination.features({ conditionalWrites: "checked" }, { changes: true }),
];

/**
 * A setting or secret's text, trimmed, when it is set and not empty.
 *
 * @param value - The raw value.
 * @returns The text, or `undefined`.
 */
function text(value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  const trimmed = value.trim();
  return trimmed === "" ? undefined : trimmed;
}

/** Stores made in this isolate, by their settings, so a sign-in is reused across calls. */
const STORES = new Map<string, B2Store>();

/**
 * Builds the destination from the instance's `CONFIG` and `SECRETS`.
 * Nothing is sent to B2 until the first storage call.
 *
 * @param env - The instance's bindings.
 * @param fetcher - How requests are sent (tests replace it).
 * @returns The destination.
 * @throws {PluginError} `invalid_params` for a missing or malformed setting
 *   or secret.
 */
export function destinationFrom(
  env: Env,
  fetcher: Fetcher = (url, init) => fetch(url, init),
): ObjectStoreDestination {
  const invalid = (what: string): PluginError =>
    PluginError.fromWire("invalid_params", `b2 destination: ${what}`);
  const config = env.CONFIG ?? {};
  const secrets = env.SECRETS ?? {};
  const bucket = text(config["bucket"]);
  if (bucket === undefined) throw invalid("the bucket is not set");
  const prefix = typeof config["prefix"] === "string" ? config["prefix"] : "";
  const keyId = text(secrets["application_key_id"]);
  const applicationKey = text(secrets["application_key"]);
  if (keyId === undefined || applicationKey === undefined) {
    throw invalid("the application key ID and application key are required");
  }
  let secret: B2SigningSecret | undefined;
  const signing = text(secrets["webhook_signing_secret"]);
  if (signing !== undefined) {
    const parsed = B2SigningSecret.parse(signing);
    if (typeof parsed === "string") throw invalid(parsed);
    secret = parsed;
  }
  const cacheKey = JSON.stringify([keyId, applicationKey, bucket, prefix]);
  let store = STORES.get(cacheKey);
  if (store === undefined) {
    try {
      store = new B2Store({ keyId, applicationKey, bucket, prefix }, { fetch: fetcher });
    } catch (err) {
      if (err instanceof PluginError) throw invalid(err.message);
      throw err;
    }
    STORES.set(cacheKey, store);
  }
  // Without KV the feed has nowhere to keep its state, so it stays off and
  // the host lists.
  const feed =
    env.KV === undefined
      ? null
      : new B2ChangeFeed({ store, kv: env.KV, webhooks: env.WEBHOOKS, webhook: WEBHOOK, secret });
  return new ObjectStoreDestination(store, feed);
}
