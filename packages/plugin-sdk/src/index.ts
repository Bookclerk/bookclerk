/**
 * `@bookclerk/plugin-sdk` — ABI types + dual-runtime entrypoints.
 *
 * Package root re-exports the workerd {@link BookclerkPlugin} base and the
 * camelCase ABI types from `generated.ts`. Prefer the dedicated subpath
 * imports when writing guests:
 *
 * - Workerd: `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"` (JS/TS)
 *   or Rust `serve` / `PluginRoot`
 * - Tools: `npx bookclerk-plugin check|fmt|package`
 * - Sparse workerd: `import { runSmoke } from "@bookclerk/plugin-sdk/sparse-workerd"`
 *
 * `BookclerkPlugin` is the guest contract. Authors export the raw class.
 *
 * See `docs/plugins.md` and `docs/code-documentation.md`.
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
  wrapPluginFromBinding,
  wrapPluginFromNative,
  PRODUCT_API_VERSION,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  MAX_LIST_PAGE,
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
  BookclerkContext,
  BookclerkPluginEnv,
  CopyResult,
  DestinationContext,
  JobContext,
  JobInvocation,
  JobOutcome,
  ListOptions,
  ListPage,
  ObjectInfo,
  ObjectMetadata,
  OidcClientTemplate,
  PluginDescribe,
  PutResult,
  ReadOptions,
  ReadResult,
  ScalarLimits,
  SourceContext,
  WorkerContext,
  WriteOptions,
} from "./plugin.js";

// JSON payload contracts generated from `schema/plugin.capnp` — the single
// source of truth. Star-exported so new payload types appear automatically.
export * from "./generated.js";
export { MAX_CHECKPOINT_BYTES } from "./abi.js";
export type { BookclerkEnv, HostBinding } from "./env.js";
