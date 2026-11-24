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
  ObjectStoreDestination,
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

import { RPC_FEATURES, destinationFrom } from "./destination.ts";

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
