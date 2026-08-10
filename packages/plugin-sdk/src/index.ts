/// <reference path="./cloudflare-workers.d.ts" />
/**
 * `@bookclerk/plugin-sdk` — ABI types + dual-runtime entrypoints.
 *
 * - Workerd: `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  `import { BookclerkPlugin, BookclerkPluginGuest } from "@bookclerk/plugin-sdk/native"`
 * - Tools: `npx bookclerk-plugin check|fmt|package`
 *
 * `BookclerkPlugin` is the guest contract on both stacks; `BookclerkPluginGuest`
 * is the native stdio Workers RPC runner (workerd uses WorkerEntrypoint hosting).
 */

export { BookclerkPlugin } from "./bookclerk-plugin.js";

export {
  API_VERSION,
  METHOD_NAMES,
  type AuthenticateUserParams,
  type BookAcquiredPayload,
  type BookclerkEnv,
  type BrandDto,
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
  type DbConnectParams,
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
  type ListDealsParams,
  type ListParams,
  type ListeningProgressPayload,
  type LoginCompleteParams,
  type LoginParams,
  type LoginStartParams,
  type MethodName,
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
