/**
 * Backblaze B2 destination (workerd guest).
 *
 * Exports the `storage` entrypoint `plugin.toml` declares, served by the
 * SDK's `ObjectStoreDestination` over the B2 store in `backend.ts`, with
 * the change feed in `feed.ts`, and the `stream_copy` job on the default
 * entrypoint. This module only reads the instance's settings and secrets.
 */
import { BookclerkEntrypoint, FEATURE_SCALAR_LIMITS, FEATURE_STREAMS, ObjectStoreDestination, PluginError, StorageEntrypoint, streamCopy, } from "@bookclerk/plugin-sdk/workerd";
import { B2Store } from "./backend.js";
import { B2SigningSecret } from "./b2-events.js";
import { B2ChangeFeed } from "./feed.js";
/** The `[[webhooks]]` row B2's event notifications post to. */
export const WEBHOOK = "changes";
/** What every B2 destination does natively, whatever its settings. */
export const RPC_FEATURES = [
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
function text(value) {
    if (typeof value !== "string")
        return undefined;
    const trimmed = value.trim();
    return trimmed === "" ? undefined : trimmed;
}
/** Stores made in this isolate, by their settings, so a sign-in is reused across calls. */
const STORES = new Map();
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
export function destinationFrom(env, fetcher = (url, init) => fetch(url, init)) {
    const invalid = (what) => PluginError.fromWire("invalid_params", `b2 destination: ${what}`);
    const config = env.CONFIG ?? {};
    const secrets = env.SECRETS ?? {};
    const bucket = text(config["bucket"]);
    if (bucket === undefined)
        throw invalid("the bucket is not set");
    const prefix = typeof config["prefix"] === "string" ? config["prefix"] : "";
    const keyId = text(secrets["application_key_id"]);
    const applicationKey = text(secrets["application_key"]);
    if (keyId === undefined || applicationKey === undefined) {
        throw invalid("the application key ID and application key are required");
    }
    let secret;
    const signing = text(secrets["webhook_signing_secret"]);
    if (signing !== undefined) {
        const parsed = B2SigningSecret.parse(signing);
        if (typeof parsed === "string")
            throw invalid(parsed);
        secret = parsed;
    }
    const cacheKey = JSON.stringify([keyId, applicationKey, bucket, prefix]);
    let store = STORES.get(cacheKey);
    if (store === undefined) {
        try {
            store = new B2Store({ keyId, applicationKey, bucket, prefix }, { fetch: fetcher });
        }
        catch (err) {
            if (err instanceof PluginError)
                throw invalid(err.message);
            throw err;
        }
        STORES.set(cacheKey, store);
    }
    // Without KV the feed has nowhere to keep its state, so it stays off and
    // the host lists.
    const feed = env.KV === undefined
        ? null
        : new B2ChangeFeed({ store, kv: env.KV, webhooks: env.WEBHOOKS, webhook: WEBHOOK, secret });
    return new ObjectStoreDestination(store, feed);
}
/** `storage` entrypoint: one line per storage method. */
export class Storage extends StorageEntrypoint {
    /** The destination, built on first use. */
    #built = null;
    /**
     * The destination for this instance.
     *
     * @returns The destination.
     */
    get #dest() {
        this.#built ??= destinationFrom(this.env);
        return this.#built;
    }
    /** @inheritDoc */
    head(key) {
        return this.#dest.head(key);
    }
    /** @inheritDoc */
    list(options) {
        return this.#dest.list(options);
    }
    /** @inheritDoc */
    get(key, options) {
        return this.#dest.get(key, options);
    }
    /** @inheritDoc */
    put(key, body, options) {
        return this.#dest.put(key, body, options);
    }
    /** @inheritDoc */
    copy(from, to) {
        return this.#dest.copy(from, to);
    }
    /** @inheritDoc */
    delete(key) {
        return this.#dest.delete(key);
    }
    /** @inheritDoc */
    commit(key, commitToken) {
        return this.#dest.commit(key, commitToken);
    }
    /** @inheritDoc */
    abortStage(key, commitToken) {
        return this.#dest.abortStage(key, commitToken);
    }
    /** @inheritDoc */
    changes(request) {
        return this.#dest.changes(request);
    }
    /** @inheritDoc */
    translateKeys(keys) {
        return this.#dest.translateKeys(keys);
    }
}
/** Identity the host merges over `plugin.toml`, and the `stream_copy` job. */
export default class B2Plugin extends BookclerkEntrypoint {
    /**
     * Describes the plugin: what B2 does natively, so the host runs its own
     * fallback for the rest.
     *
     * @returns Display name and features.
     */
    describe() {
        return Promise.resolve({ displayName: "Backblaze B2", rpcFeatures: RPC_FEATURES });
    }
    /**
     * Runs the `stream_copy` job: copies an object from the job's input into
     * this destination.
     *
     * @param controller - The job.
     * @returns The completion.
     */
    job(controller) {
        return streamCopy(controller);
    }
}
