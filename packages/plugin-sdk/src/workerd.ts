/**
 * Workerd / Workers RPC guest entry — re-exports {@link BookclerkEntrypoint}
 * and the named `*Entrypoint` bases.
 *
 * Import from `@bookclerk/plugin-sdk/workerd` inside Cloudflare workerd
 * isolates. The host (`bookclerk-workerd`) injects this module at serve time.
 *
 * @example
 * ```ts
 * import {
 *   BookclerkEntrypoint,
 *   CliEntrypoint,
 *   cliArgs,
 *   jsonPayload,
 *   type CliInvokeParams,
 *   type EventBatch,
 *   type JobController,
 * } from "@bookclerk/plugin-sdk/workerd";
 * import type { Env } from "./bookclerk-configuration.js";
 *
 * export class Cli extends CliEntrypoint<Env> {
 *   async describe() {
 *     return { commands: [{ name: "ping", about: "Probe", args: [] }] };
 *   }
 *   async invoke(params: CliInvokeParams) {
 *     const { message = "hi" } = cliArgs(params);
 *     return { exitCode: 0, stdout: `pong ${message}\n`, stderr: "", payload: jsonPayload({ message }) };
 *   }
 * }
 *
 * export default class MyPlugin extends BookclerkEntrypoint<Env> {
 *   async event(batch: EventBatch) {
 *     for (const msg of batch.messages) msg.ack();
 *   }
 *   async job(job: JobController) {
 *     await job.progress(100, "done");
 *     return { message: "ok" };
 *   }
 * }
 * ```
 */

import "./cloudflare-workers.d.ts";

export {
  AdapterDatabaseSession,
  BookclerkEntrypoint,
  CliEntrypoint,
  DatabaseAdapterEntrypoint,
  Destination,
  EventBatch,
  EventMessage,
  JobController,
  NamedEntrypoint,
  OidcEntrypoint,
  PluginError,
  ProgressSink,
  RemoteLibraryEntrypoint,
  Source,
  StorageEntrypoint,
  StorefrontEntrypoint,
  wrapPluginFromBinding,
  wrapPluginFromNative,
  allowedEntrypointFamilies,
  ENTRYPOINT_BINDINGS,
  ENTRYPOINT_CLASSES,
  TRIGGER_FAMILIES,
  cliArgs,
  decodeExtensibleConfig,
  jsonPayload,
  eventBatchResults,
  invocationOf,
  jobOutcomeFor,
  fromBridgeJson,
  toBridgeJson,
  schemaMigrationOp,
  dataMigrationOp,
  requirePluginMigrationRegistration,
  PRODUCT_API_VERSION,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  MAX_LIST_PAGE,
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STREAMS,
  FEATURE_STORAGE_COPY,
} from "./plugin.js";
export type {
  AdapterEnv,
  Checkpoint,
  CopyResult,
  DeliveredCheckpoint,
  DomainEvent,
  EntrypointName,
  EventOutcome,
  EventSuspendOptions,
  GrantedContext,
  GrantedDatabaseTransport,
  NamedDatabase,
  Instant,
  Invocation,
  JobCheckpoint,
  JobCompletion,
  JobInvocation,
  JobOutcomeRecord,
  JobRetryOptions,
  JobRunnerContext,
  JobSuspendOptions,
  ListOptions,
  ListPage,
  ObjectInfo,
  ObjectMetadata,
  OidcClientTemplate,
  PluginCapabilities,
  PluginDescribe,
  PluginMigration,
  PluginMigrationOp,
  PutResult,
  ReadOptions,
  ReadResult,
  RetryOptions,
  WriteOptions,
} from "./plugin.js";
export type {
  BookclerkEnv,
  EventPublisherBinding,
  JsonObject,
  PublishEvent,
  StorageBinding,
} from "./env.js";
export { InvokeError, dispatchInvoke } from "./invoke.js";
export type { CapDescriptor, InvokeFailure, InvokeHost, InvokeOutcome, InvokeReply } from "./invoke.js";
export type {
  CliSchema,
  CliInvokeParams,
  CliInvokeResult,
  ExtensibleConfig,
  HealthOk,
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
