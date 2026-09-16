/**
 * `@bookclerk/plugin-sdk` — ABI types + workerd author runtime + tools.
 *
 * Package root re-exports the workerd {@link BookclerkEntrypoint} base, the
 * named `*Entrypoint` bases, and the camelCase ABI types from `generated.ts`.
 * Prefer the dedicated subpath imports when writing guests:
 *
 * - Workerd: `import { BookclerkEntrypoint } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  Rust `serve` / `PluginWorker`
 * - Tools: `npx bookclerk-plugin check|fmt|types|package`
 * - Sparse workerd: `import { runSmoke } from "@bookclerk/plugin-sdk/sparse-workerd"`
 *
 * See `docs/plugins.md` and `docs/code-documentation.md`.
 */

import "./cloudflare-workers.d.ts";

export {
  AdapterDatabaseSession,
  BookclerkEntrypoint,
  CliEntrypoint,
  DatabaseAdapterEntrypoint,
  Destination,
  ENTRYPOINT_BINDINGS,
  ENTRYPOINT_CLASSES,
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
  cliArgs,
  decodeExtensibleConfig,
  jsonPayload,
  eventBatchResults,
  invocationOf,
  jobOutcomeFor,
  wrapPluginFromBinding,
  wrapPluginFromNative,
  schemaMigrationOp,
  dataMigrationOp,
  requirePluginMigrationRegistration,
  toBridgeJson,
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
} from "./plugin.js";
export { decodeDbValue, encodeDbValue, parseDbValue } from "./db-value.js";
export type { DbType, DbValue } from "./db-value.js";
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
export type {
  AdapterEnv,
  AuthorStub,
  CancelWatchLike,
  Checkpoint,
  CopyResult,
  DeliveredCheckpoint,
  EntrypointName,
  EventOutcome,
  EventSuspendOptions,
  GrantedContext,
  GrantedFetcher,
  GrantedJobCapabilities,
  Instant,
  JobCheckpoint,
  JobCompletion,
  JobOutcomeRecord,
  JobRetryOptions,
  JobRunnerContext,
  JobSuspendOptions,
  ListOptions,
  ListPage,
  NamedStub,
  ObjectInfo,
  ObjectMetadata,
  PluginDescribe,
  PutResult,
  ReadOptions,
  ReadResult,
  RetryOptions,
  ScalarLimits,
  WireEventBatch,
  WriteOptions,
} from "./plugin.js";

// Every struct, union, and interface of `schema/plugin.capnp` — the single
// source of truth — projected with TSDoc by `scripts/gen-plugin-abi.py`.
// Star-exported so new ABI types appear automatically.
export * from "./generated.js";
export { MAX_CHECKPOINT_BYTES } from "./abi.js";
export type {
  BookclerkEnv,
  EventPublisherBinding,
  JsonObject,
  PublishEvent,
  StorageBinding,
} from "./env.js";
