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
 * See `docs/plugins/README.md` and `docs/contributing/code-documentation.md`.
 *
 * @module plugin-sdk
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
  QueueBatch,
  QueueMessage,
  JobController,
  CoverEntrypoint,
  MetadataEntrypoint,
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
  queueBatchResults,
  invocationOf,
  jobOutcomeFor,
  wrapPluginFromBinding,
  wrapPluginFromNative,
  allowedEntrypointFamilies,
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
export { limitExceededMessage, parseLimitExceeded } from "./errors.js";
export type { LimitExceeded } from "./errors.js";
export {
  ExecuteStreamDecoder,
  collectExecuteStream,
  decodeExecuteStream,
  executeStreamLimits,
  rawStreamCeiling,
  streamsResults,
} from "./db-stream.js";
export type { ExecuteStreamLimits } from "./db-stream.js";
export {
  GUEST_READ_ONLY_REPLAY_PAYLOAD,
  GUEST_RECEIPT_CLAIM_LOST,
  GUEST_RECEIPT_RESULT_LOST,
  GUEST_RECEIPT_STUB_SUFFIX,
  GUEST_RECEIPT_WRAP_PREFIX,
  GUEST_REPLAY_PAYLOAD_MAX_BYTES,
  STREAM_FRAME_TARGET_BYTES,
  encodeExecuteStream,
  executeAdapterBatch,
  executeAdapterBatchStreamed,
  guestBatchIsReadOnly,
  guestReceiptFinalizeStatements,
  guestReplayPayload,
  runAdapterBatch,
  shouldSkipRemainingGuestWork,
} from "./db-adapter.js";
export type {
  AdapterBatchOptions,
  AdapterBatchStreamedOptions,
  AdapterBatchRequest,
  AdapterBatchStatement,
  AdapterTransaction,
} from "./db-adapter.js";
export {
  canonicalExecuteRequestHash,
  chunkIndexes,
  createDatabaseBinding,
  decodeDbCapabilitiesReply,
  decodeExecuteResultReply,
  decodeExecuteRequest,
  encodeDbCapabilitiesReply,
  encodeExecuteResultReply,
  encodeExecuteRequest,
  executeReplyToD1Results,
  MAX_WRITE_KEY_BYTES,
  statementResultToD1Result,
  WriteChunksError,
  writeChunksOperationId,
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
  WriteChunksOptions,
  WriteProgress,
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
  GrantedDatabaseTransport,
  NamedDatabase,
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
  NamedKv,
  NamedQueue,
  QueueOutcome,
  QueueRetryOptions,
  WireEventBatch,
  WireQueueBatch,
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
  KvListKeyBinding,
  KvListOptionsBinding,
  KvListResultBinding,
  KvNamespaceBinding,
  KvPutOptionsBinding,
  KvValueType,
  KvValueWithMetadata,
  PublishEvent,
  QueueBinding,
  QueueSendBatchEntry,
  QueueSendOptions,
  StorageBinding,
} from "./env.js";
