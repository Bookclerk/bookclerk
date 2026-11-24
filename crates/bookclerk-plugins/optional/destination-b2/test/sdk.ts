/**
 * Replaces the SDK's isolate module for tests. The helpers the guest uses
 * are the SDK's real modules, loaded one by one (the package's entries
 * import `cloudflare:workers`, which only exists inside workerd), and the
 * entrypoint base classes are stand-ins that keep their bindings.
 *
 * Import this module before importing anything from `src/`.
 */

import { mock } from "node:test";

import * as abi from "../node_modules/@bookclerk/plugin-sdk/dist/abi.js";
import * as errors from "../node_modules/@bookclerk/plugin-sdk/dist/errors.js";
import * as objectStore from "../node_modules/@bookclerk/plugin-sdk/dist/object-store.js";
import * as queueFeed from "../node_modules/@bookclerk/plugin-sdk/dist/queue-feed.js";
import * as streamCopy from "../node_modules/@bookclerk/plugin-sdk/dist/stream-copy.js";

/** The SDK's real exports the guest uses. */
const sdk = { ...abi, ...errors, ...objectStore, ...queueFeed, ...streamCopy };

/** The SDK's error class, as the guest sees it. */
export const { PluginError } = errors;

/** Stand-in for the SDK's entrypoint base classes. */
export class Entrypoint {
  /** Bindings. */
  readonly env: unknown;

  /**
   * Keeps the bindings, as `WorkerEntrypoint` does.
   *
   * @param _ctx - Execution context (unused).
   * @param env - Bindings.
   */
  constructor(_ctx: unknown, env: unknown) {
    this.env = env;
  }
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  namedExports: { ...sdk, BookclerkEntrypoint: Entrypoint, StorageEntrypoint: Entrypoint },
});
