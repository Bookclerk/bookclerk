/**
 * Backblaze B2 destination (workerd guest).
 *
 * Exports the `storage` entrypoint `plugin.toml` declares, served by the
 * SDK's `ObjectStoreDestination` over the B2 store in `backend.ts`, with
 * the change feed in `feed.ts`, and the `stream_copy` job on the default
 * entrypoint. This module only reads the instance's settings and secrets.
 */

import {
  BookclerkEntrypoint,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  ObjectStoreDestination,
  PluginError,
  StorageEntrypoint,
  streamCopy,
  type ByteRange,
  type ChangesPage,
  type ChangesRequest,
  type CopyResult,
  type JobCompletion,
  type JobController,
  type ListOptions,
  type ListPage,
  type ObjectMetadata,
  type PluginDescribe,
  type PutResult,
  type ReadResult,
  type WriteOptions,
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

/** `storage` entrypoint: one line per storage method. */
export class Storage extends StorageEntrypoint<Env> {
  /** The destination, built on first use. */
  #built: ObjectStoreDestination | null = null;

  /**
   * The destination for this instance.
   *
   * @returns The destination.
   */
  get #dest(): ObjectStoreDestination {
    this.#built ??= destinationFrom(this.env);
    return this.#built;
  }

  /** @inheritDoc */
  override head(key: string): Promise<ObjectMetadata | null> {
    return this.#dest.head(key);
  }
  /** @inheritDoc */
  override list(options: ListOptions): Promise<ListPage> {
    return this.#dest.list(options);
  }
  /** @inheritDoc */
  override get(key: string, options?: { range?: ByteRange }): Promise<ReadResult> {
    return this.#dest.get(key, options);
  }
  /** @inheritDoc */
  override put(
    key: string,
    body: ReadableStream<Uint8Array>,
    options?: WriteOptions,
  ): Promise<PutResult> {
    return this.#dest.put(key, body, options);
  }
  /** @inheritDoc */
  override copy(from: string, to: string): Promise<CopyResult> {
    return this.#dest.copy(from, to);
  }
  /** @inheritDoc */
  override delete(key: string): Promise<void> {
    return this.#dest.delete(key);
  }
  /** @inheritDoc */
  override commit(key: string, commitToken: string): Promise<PutResult> {
    return this.#dest.commit(key, commitToken);
  }
  /** @inheritDoc */
  override abortStage(key: string, commitToken: string): Promise<void> {
    return this.#dest.abortStage(key, commitToken);
  }
  /** @inheritDoc */
  override changes(request: ChangesRequest): Promise<ChangesPage> {
    return this.#dest.changes(request);
  }
  /** @inheritDoc */
  override translateKeys(keys: string[]): Promise<string[]> {
    return this.#dest.translateKeys(keys);
  }
}

/** Identity the host merges over `plugin.toml`, and the `stream_copy` job. */
export default class B2Plugin extends BookclerkEntrypoint<Env> {
  /**
   * Describes the plugin: what B2 does natively, so the host runs its own
   * fallback for the rest.
   *
   * @returns Display name and features.
   */
  override describe(): Promise<PluginDescribe> {
    return Promise.resolve({ displayName: "Backblaze B2", rpcFeatures: RPC_FEATURES });
  }

  /**
   * Runs the `stream_copy` job: copies an object from the job's input into
   * this destination.
   *
   * @param controller - The job.
   * @returns The completion.
   */
  override job(controller: JobController): Promise<JobCompletion> {
    return streamCopy(controller);
  }
}
