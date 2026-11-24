/**
 * Backblaze B2 destination (workerd guest).
 *
 * Exports the `storage` entrypoint `plugin.toml` declares, served by the
 * SDK's `ObjectStoreDestination` over the B2 store in `backend.ts`, with
 * the change feed in `feed.ts`, and the `stream_copy` job on the default
 * entrypoint. This module only reads the instance's settings and secrets.
 */
import { BookclerkEntrypoint, StorageEntrypoint, streamCopy, } from "@bookclerk/plugin-sdk/workerd";
import { RPC_FEATURES, destinationFrom } from "./destination.js";
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
