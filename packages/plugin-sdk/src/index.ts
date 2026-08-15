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
  CopyResult,
  DestinationContext,
  JobContext,
  JobInvocation,
  JobOutcome,
  ListOptions,
  ListPage,
  ObjectInfo as V2ObjectInfo,
  ObjectMetadata,
  PluginDescribe,
  PutResult,
  ReadOptions,
  ReadResult,
  ScalarLimits,
  SourceContext,
  WorkerContext,
  WriteOptions,
} from "./v2.js";

export {
  API_VERSION,
  METHOD_NAMES,
  type AuthenticateUserParams,
  type BookAcquiredPayload,
  type BookclerkEnv,
  type BrandDto,
  type CatalogDetailParams,
  type CliArgKind,
  type CliArgSpec,
  type CliCommandSpec,
  type CliInvokeParams,
  type CliInvokeResult,
  type CliSchema,
  type ConfigChangedPayload,
  type ConfigOptionDto,
  type ConfigOptionValueDto,
  type CopyParams,
  type CredentialsUpdateParams,
  type DbBeginParams,
  type DbBeginResult,
  type DbConnectParams,
  type DbTxnParams,
  type DiagnoseResult,
  type ExpandCandidatesParams,
  type ExternalUsersPayload,
  type FetchTitleParams,
  type HandshakeParams,
  type HandshakeResult,
  type HealthResult,
  type HostBinding,
  type HostToPluginEvent,
  type JsonObject,
  type KeyParams,
  type LibraryScanCompletedPayload,
  type ListAccountsParams,
  type ListDealsParams,
  type ListParams,
  type ListeningProgressPayload,
  type LoginCompleteParams,
  type LoginParams,
  type LoginStartParams,
  type MethodName,
  type ObjectMetaDto,
  type OutputS3Context,
  type PluginError as AbiPluginError,
  type PluginErrorCode,
  type PluginKind,
  type PluginLogLevel,
  type PluginLogPayload,
  type PluginToHostEvent,
  type PurchaseHintParams,
  type PutFileParams,
  type PutParams,
  type ScanLibraryParams,
  type ScanParams,
  type SearchCatalogParams,
  type StatementDto,
  type TouchFileParams,
} from "./generated.js";
