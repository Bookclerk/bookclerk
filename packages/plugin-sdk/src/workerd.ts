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
 *   async describe(): Promise<PluginDescribe> {
 *     return { apiVersion: 2, id: "my_plugin", kind: "source", rpcFeatures: [], scalarLimits: { maxScalarBytes: 262144, maxStreamWindowBytes: 1048576, maxListPage: 256 } };
 *   }
 * }
 * ```
 */

import "./cloudflare-workers.d.ts";

export {
  BookclerkPlugin,
  ContentSource,
  Destination,
  Integration,
  JobHandler,
  PluginError,
  ProgressSink,
  Source,
  wrapV2PluginFromBinding,
  wrapV2PluginFromNative,
  PRODUCT_API_VERSION,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  MAX_LIST_PAGE,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  FEATURE_STORAGE_COPY,
  ENVELOPE_VERSION,
} from "./v2.js";
export type {
  AdapterEnv,
  BookclerkContext,
  BookclerkPluginEnv,
  DestinationContext,
  DomainEvent,
  EventResult,
  JobContext,
  JobInvocation,
  JobOutcome,
  PluginDescribe,
  WorkerContext,
} from "./v2.js";
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
