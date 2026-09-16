/**
 * Workerd runtime binding surfaces available on the guest `env`.
 *
 * These are runtime capability shapes (they carry functions), not wire DTOs,
 * so they live here rather than in the generated `generated.ts` projection.
 */

import type { JsonObject } from "./generated.js";

/**
 * Host binding used by guests to push plugin → host notifications.
 */
export interface HostBinding {
  /**
   * Delivers a plugin → host event on the reverse notify channel.
   *
   * @param event - JSON event envelope to send.
   * @returns Resolves when the host acknowledges the notify.
   */
  notify(event: JsonObject): Promise<void>;
}

/**
 * Guest `env` bindings declared by the ABI / `capabilities.bindings`.
 *
 * Only `HOST` is required for event push; other bindings appear when the
 * operator has consented to them in `plugin.toml`.
 */
export interface BookclerkEnv {
  /** Reverse channel for plugin → host events. */
  HOST: HostBinding;
  /** Operator config object when the `config` binding is enabled. */
  CONFIG?: JsonObject;
  /** Sealed secrets binding when the `secrets` capability is enabled. */
  SECRETS?: unknown;
  /** Per-plugin KV store when the `plugin_kv` binding is enabled. */
  PLUGIN_KV?: unknown;
  /** Work filesystem binding when the `work_fs` capability is enabled. */
  WORK_FS?: unknown;
  /** OAuth helper binding when the `oauth` capability is enabled. */
  OAUTH?: unknown;
}
