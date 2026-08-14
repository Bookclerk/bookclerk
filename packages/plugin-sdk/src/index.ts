/**
 * `@bookclerk/plugin-sdk` — ABI types + dual-runtime entrypoints.
 *
 * Package root re-exports the workerd {@link BookclerkPlugin} base and the
 * camelCase ABI types from `generated.ts`. Prefer the dedicated subpath
 * imports when writing guests:
 *
 * - Workerd: `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  `import { BookclerkPlugin, BookclerkPluginGuest } from "@bookclerk/plugin-sdk/native"`
 * - Tools: `npx bookclerk-plugin check|fmt|package`
 * - Sparse workerd: `import { runSmoke } from "@bookclerk/plugin-sdk/sparse-workerd"`
 *
 * `BookclerkPlugin` is the guest contract on both stacks; `BookclerkPluginGuest`
 * is the native stdio Workers RPC runner (workerd uses WorkerEntrypoint hosting).
 *
 * See `docs/plugins.md` and `docs/code-documentation.md`.
 */

import "./cloudflare-workers.d.ts";

export { BookclerkPlugin } from "./bookclerk-plugin.js";

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
  type PluginError,
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
