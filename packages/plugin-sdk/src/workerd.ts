/**
 * Workerd / Workers RPC guest entry — re-exports {@link BookclerkPlugin}.
 *
 * Import from `@bookclerk/plugin-sdk/workerd` inside Cloudflare workerd
 * isolates. The host (`bookclerk-workerd`) injects this module at serve time.
 *
 * @example
 * ```ts
 * import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
 * import type { HandshakeParams, HandshakeResult } from "@bookclerk/plugin-sdk/workerd";
 *
 * export default class MyPlugin extends BookclerkPlugin {
 *   async handshake(_params: HandshakeParams): Promise<HandshakeResult> {
 *     return { apiVersion: 1, id: "my_plugin", kind: "source", capabilities: [] };
 *   }
 * }
 * ```
 */

import "./cloudflare-workers.d.ts";

export { BookclerkPlugin } from "./bookclerk-plugin.js";
export type {
  BookclerkEnv,
  HandshakeParams,
  HandshakeResult,
  HealthResult,
  DiagnoseResult,
  CliSchema,
  CliInvokeParams,
  CliInvokeResult,
  HostToPluginEvent,
} from "./generated.js";
