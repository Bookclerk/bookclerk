/**
 * Workerd / Workers RPC guest entry — re-exports {@link BookclerkPlugin}.
 *
 * Import from `@bookclerk/plugin-sdk/workerd` inside Cloudflare workerd
 * isolates. The host (`bookclerk-workerd`) injects this module at serve time.
 *
 * @example
 * ```ts
 * import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
 * import type { PluginDescribe } from "@bookclerk/plugin-sdk/workerd";
 *
 * export default class MyPlugin extends BookclerkPlugin {
 *   async describe(): Promise<PluginDescribe> {
 *     return {
 *       apiVersion: 2,
 *       id: "my_plugin",
 *       kind: "source",
 *       rpcFeatures: [],
 *       scalarLimits: { maxScalarBytes: 262144, maxStreamWindowBytes: 1048576, maxListPage: 256 },
 *       capabilities: ["health", "login", "scan", "fetchTitle"],
 *     };
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
  AdapterDatabaseSession,
  GuestDatabase,
  wrapPluginFromBinding,
  wrapPluginFromNative,
  PRODUCT_API_VERSION,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  FEATURE_STORAGE_COPY,
  requirePluginMigrationRegistration,
} from "./plugin.js";
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
  OidcClientTemplate,
  PluginDescribe,
  WorkerContext,
} from "./plugin.js";
export type { BookclerkEnv } from "./env.js";
export type {
  CliSchema,
  CliInvokeParams,
  CliInvokeResult,
  ContentSourceContext,
  DatabaseContext,
  ExtensibleConfig,
  HealthOk,
  IntegrationContext,
  SourceContext,
} from "./generated.js";
export {
  canonicalExecuteRequestHash,
  createDatabaseBinding,
  decodeExecuteResultReply,
  decodeExecuteRequest,
  encodeExecuteResultReply,
  encodeExecuteRequest,
  executeReplyToD1Results,
  statementResultToD1Result,
} from "./db-execute.js";
export { decodeDbValue, encodeDbValue, parseDbValue } from "./db-value.js";
export type { DbType, DbValue } from "./db-value.js";
export type {
  AtomicTransport,
  DatabaseBinding,
  DatabaseBindingOptions,
  D1ExecResult,
  D1Meta,
  D1Result,
  DbColumn,
  DbResultSelection,
  DbRow,
  DbStatementKind,
  DbTiming,
  ExecuteReply,
  ExecuteRequest,
  PreparedStatement,
  RetryToken,
  StatementResult,
  TypedDbStatement,
} from "./db-execute.js";
