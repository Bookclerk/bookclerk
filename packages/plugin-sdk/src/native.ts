/**
 * Native (stdio) guest — same branded {@link BookclerkPlugin} contract as workerd.
 *
 * Dual-stack entry:
 * - Workerd: `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  `import { BookclerkPlugin, BookclerkPluginGuest } from "@bookclerk/plugin-sdk/native"`
 *
 * {@link BookclerkPluginGuest} is the native stdin/stdout Workers RPC runner
 * (workerd hosts the class via WorkerEntrypoint instead). Authors subclass
 * {@link BookclerkPlugin}; plain objects with the same methods also work via
 * {@link BookclerkPluginLike}.
 */

import type {
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  DiagnoseResult,
  HandshakeParams,
  HandshakeResult,
  HealthResult,
  HostToPluginEvent,
} from "./generated.js";

/**
 * Structural guest contract shared by subclasses and duck-typed objects.
 *
 * Prefer extending {@link BookclerkPlugin}. Every optional method defaults to
 * unsupported when omitted from a plain object served by
 * {@link BookclerkPluginGuest.serve}.
 */
export type BookclerkPluginLike = {
  /**
   * Negotiates ABI version and advertises plugin identity to the host.
   *
   * @param params - Host-provided handshake inputs (`apiVersion`, `config`).
   * @returns Guest identity, kind, and capability method names.
   */
  handshake(
    params: HandshakeParams,
  ): Promise<HandshakeResult> | HandshakeResult;
  /**
   * Releases guest resources before the process exits.
   *
   * @returns Resolves when teardown is complete (may be synchronous).
   */
  shutdown?(): Promise<void> | void;
  /**
   * Reports liveness for host health checks.
   *
   * @returns Health payload; hosts treat a missing implementation as `{ ok: true }`.
   */
  health?(): Promise<HealthResult> | HealthResult;
  /**
   * Collects operator-facing diagnostic lines for `plugins doctor`.
   *
   * @returns Diagnostic payload; hosts treat a missing implementation as `{ lines: [] }`.
   */
  diagnose?(): Promise<DiagnoseResult> | DiagnoseResult;
  /**
   * Handles a host → plugin push event (`book_acquired`, `config_changed`, …).
   *
   * @param event - Discriminated event envelope from the host.
   * @returns Resolves when the guest has finished handling the event.
   */
  onEvent?(event: HostToPluginEvent): Promise<void> | void;
  /**
   * Describes the guest CLI surface (mirrors handshake `cli` when present).
   *
   * @returns CLI schema; hosts treat a missing implementation as `{ commands: [] }`.
   */
  cliDescribe?(): Promise<CliSchema> | CliSchema;
  /**
   * Runs a plugin CLI command advertised by {@link BookclerkPluginLike.cliDescribe}.
   *
   * @param params - Command name and argument map from the host.
   * @returns Exit code, captured stdout/stderr, and optional JSON payload.
   */
  cliInvoke?(params: CliInvokeParams): Promise<CliInvokeResult> | CliInvokeResult;
  /**
   * Starts long-running guest work after a successful handshake.
   *
   * @param params - Optional kind-specific start parameters from the host.
   * @returns Resolves when startup work has been accepted.
   */
  start?(params?: unknown): Promise<void> | void;
  /**
   * Drains queued plugin → host events for integration guests.
   *
   * @returns Queued events (shape is kind-specific; often user / progress batches).
   */
  pollEvents?(): Promise<unknown> | unknown;
  /**
   * Scans the operator library through an integration guest.
   *
   * @param params - Scan scope and account filters from the host.
   * @returns Resolves when the scan side effects are complete.
   */
  scanLibrary?(params: unknown): Promise<void> | void;
  /**
   * Syncs listening progress with an external library server.
   *
   * @returns Sync summary (kind-specific).
   */
  syncListening?(): Promise<unknown> | unknown;
  /**
   * Validates an external user identity for portal / OIDC flows.
   *
   * @param params - Credentials or tokens from the connect portal.
   * @returns Authentication result (kind-specific).
   */
  authenticateUser?(params: unknown): Promise<unknown> | unknown;
  /**
   * Performs a one-shot store login (password / token flows).
   *
   * @param params - Store credentials and account labeling (`pluginDataDir`, …).
   * @returns Login result including account metadata and opaque credentials.
   */
  login?(params: unknown): Promise<unknown> | unknown;
  /**
   * Starts an interactive OAuth (or similar) login.
   *
   * @param params - Marketplace / locale hints for the authorize URL.
   * @returns Login-start result including `sessionId` and browser `url` when applicable.
   */
  loginStart?(params: unknown): Promise<unknown> | unknown;
  /**
   * Completes an interactive login started by {@link BookclerkPluginLike.loginStart}.
   *
   * @param params - Callback payload / authorization code (`sessionId`, …).
   * @returns Login result (kind-specific).
   */
  loginComplete?(params: unknown): Promise<unknown> | unknown;
  /**
   * Updates stored credentials without a full login round-trip.
   *
   * @param params - Account id and replacement secret material for the host to re-seal.
   * @returns Resolves when the guest has finished requesting the write-back.
   */
  credentialsUpdate?(params: unknown): Promise<void> | void;
  /**
   * Scans owned titles from a source storefront.
   *
   * @param params - Account filters, pagination, and host-injected credentials.
   * @returns Scan summary including titles for the host to upsert.
   */
  scan?(params: unknown): Promise<unknown> | unknown;
  /**
   * Downloads (and decrypts, when applicable) one title to a fetch directory.
   *
   * @param params - Title id / ASIN / ISBN, cache directory, and credential blob.
   * @returns Fetch result describing acquired plain media paths.
   */
  fetchTitle?(params: unknown): Promise<unknown> | unknown;
  /**
   * Searches the storefront catalog.
   *
   * @param params - Query string and optional region / pagination filters.
   * @returns Catalog hits for the host UI.
   */
  searchCatalog?(params: unknown): Promise<unknown> | unknown;
  /**
   * Expands related catalog candidates for a seed title.
   *
   * @param params - Seed identifiers (ASIN/ISBN/title) and expansion limits.
   * @returns Related catalog hits.
   */
  expandCandidates?(params: unknown): Promise<unknown> | unknown;
  /**
   * Returns a purchase / wishlist hint for a catalog title when available.
   *
   * @param params - Title identifier in the storefront namespace.
   * @returns Purchase hint or nullish when unavailable.
   */
  purchaseHint?(params: unknown): Promise<unknown> | unknown;
  /**
   * Lists current deals / sales from the storefront.
   *
   * @param params - Pagination and marketplace filters.
   * @returns Catalog hits for deals.
   */
  listDeals?(params: unknown): Promise<unknown> | unknown;
  /**
   * Lists connected source accounts known to this guest.
   *
   * @param params - Optional account-id filter (may be an empty object).
   * @returns Account descriptors for the Accounts UI.
   */
  listAccounts?(params: unknown): Promise<unknown> | unknown;
  /**
   * Fetches rich catalog detail for one title.
   *
   * @param params - Storefront title identifier (`productId` / `isbn`).
   * @returns Catalog detail or nullish when missing.
   */
  catalogDetail?(params: unknown): Promise<unknown> | unknown;
  /**
   * Writes bytes to a destination object key.
   *
   * @param params - Object key and inline Base64 payload (plus S3/local context).
   * @returns Resolves when the object has been written.
   */
  put?(params: unknown): Promise<void> | void;
  /**
   * Uploads a local file to a destination object key.
   *
   * @param params - Object key and absolute source path (or side-channel FD).
   * @returns Resolves when the upload completes.
   */
  putFile?(params: unknown): Promise<void> | void;
  /**
   * Reads an object from the destination.
   *
   * @param params - Object key and optional byte range / destination context.
   * @returns Object bytes (often Base64) or metadata (kind-specific).
   */
  get?(params: unknown): Promise<unknown> | unknown;
  /**
   * Tests whether a destination object key exists.
   *
   * @param params - Object key and destination context.
   * @returns True when the object exists.
   */
  exists?(params: unknown): Promise<boolean> | boolean;
  /**
   * Lists objects under a destination prefix.
   *
   * @param params - Prefix and destination context / pagination options.
   * @returns Listing result (kind-specific).
   */
  list?(params: unknown): Promise<unknown> | unknown;
  /**
   * Probes destination connectivity / credentials.
   *
   * @param params - Probe options and destination context.
   * @returns Probe result describing reachability.
   */
  probe?(params: unknown): Promise<unknown> | unknown;
  /**
   * Copies an object within the destination.
   *
   * @param params - Source and destination keys plus destination context.
   * @returns Resolves when the copy completes.
   */
  copy?(params: unknown): Promise<void> | void;
  /**
   * Deletes an object from the destination.
   *
   * @param params - Object key and destination context.
   * @returns Resolves when the delete completes.
   */
  delete?(params: unknown): Promise<void> | void;
  /**
   * Updates mtime / metadata for a destination object without rewriting bytes.
   *
   * @param params - Object key, optional created/modified timestamps, and context.
   * @returns Resolves when metadata has been updated.
   */
  touchFile?(params: unknown): Promise<void> | void;
  /**
   * Opens a database session for a database-kind guest.
   *
   * @param params - Connection / DSN options (`backend`, paths, tokens).
   * @returns Connection handle or session descriptor.
   */
  dbConnect?(params: unknown): Promise<unknown> | unknown;
  /**
   * Pings an open database session.
   *
   * @returns Resolves when the backend responds successfully.
   */
  dbPing?(): Promise<void> | void;
  /**
   * Runs a read query against the database guest.
   *
   * @param params - SQL statement and bind parameters.
   * @returns Query rows / result set.
   */
  dbQuery?(params: unknown): Promise<unknown> | unknown;
  /**
   * Runs a write / execute statement against the database guest.
   *
   * @param params - SQL statement and bind parameters.
   * @returns Execute result (rows affected, last insert id, etc.).
   */
  dbExecute?(params: unknown): Promise<unknown> | unknown;
  /**
   * Begins a database transaction (or nested savepoint).
   *
   * @param params - Optional `parentTxnId` for nested savepoints.
   * @returns `{ txnId }` for subsequent statements.
   */
  dbBegin?(params: unknown): Promise<unknown> | unknown;
  /**
   * Commits a guest transaction returned by {@link BookclerkPlugin.dbBegin}.
   *
   * @param params - `{ txnId }`.
   */
  dbCommit?(params: unknown): Promise<void> | void;
  /**
   * Rolls back a guest transaction returned by {@link BookclerkPlugin.dbBegin}.
   *
   * @param params - `{ txnId }`.
   */
  dbRollback?(params: unknown): Promise<void> | void;
  /**
   * Runs a named atomic library operation as one guest SQL transaction.
   *
   * D1 implements this as one HTTP `batch()`. SQLite / Postgres leave it
   * unimplemented; the host uses interactive `dbBegin` on those backends.
   *
   * @param params - Tagged `{ op, ... }` operation.
   * @returns `{ status, payload? }`.
   */
  dbAtomic?(params: unknown): Promise<unknown> | unknown;
  /**
   * Fallback dispatcher for unknown wire method names.
   *
   * Prefer declaring known methods on the guest; hosts call this only when the
   * method is absent from the fixed dispatch table.
   *
   * @param method - Wire method name from the host frame.
   * @param params - Raw params object from the host (may be undefined).
   * @returns Method-specific result for the host.
   */
  callRaw?(method: string, params: unknown): Promise<unknown> | unknown;
};

/**
 * Branded native guest base — method surface matches workerd `BookclerkPlugin`.
 *
 * Subclass and override the methods declared in `capabilities.methods`. Defaults
 * that throw carry `code: "unsupported"` so the host can map them to
 * `PluginErrorCode`.
 *
 * @example
 * ```ts
 * class Echo extends BookclerkPlugin {
 *   handshake() {
 *     return { apiVersion: 1, id: "echo", kind: "source", capabilities: [] };
 *   }
 * }
 * await BookclerkPluginGuest.serve(new Echo());
 * ```
 */
export abstract class BookclerkPlugin implements BookclerkPluginLike {
  /**
   * Negotiates ABI version and advertises plugin identity to the host.
   *
   * @param params - Host-provided handshake inputs (API version, config).
   * @returns Guest identity, kind, and negotiated capabilities.
   */
  abstract handshake(
    params: HandshakeParams,
  ): Promise<HandshakeResult> | HandshakeResult;

  /**
   * Releases guest resources before the process exits.
   *
   * @returns Resolves when teardown is complete.
   */
  async shutdown(): Promise<void> {}

  /**
   * Reports liveness for host health checks.
   *
   * @returns Health payload; default is `{ ok: true }`.
   */
  async health(): Promise<HealthResult> {
    return { ok: true };
  }

  /**
   * Collects operator-facing diagnostic lines for `plugins doctor`.
   *
   * @returns Diagnostic lines; default is an empty list.
   */
  async diagnose(): Promise<DiagnoseResult> {
    return { lines: [] };
  }

  /**
   * Handles a host → plugin push event.
   *
   * @param _event - Event envelope from the host.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async onEvent(_event: HostToPluginEvent): Promise<void> {
    throw unsupported("onEvent");
  }

  /**
   * Describes the guest CLI surface.
   *
   * @returns CLI schema; default has no commands.
   */
  async cliDescribe(): Promise<CliSchema> {
    return { commands: [] };
  }

  /**
   * Runs a plugin CLI command.
   *
   * @param _params - Command name and argument map.
   * @returns CLI invocation result.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async cliInvoke(_params: CliInvokeParams): Promise<CliInvokeResult> {
    throw unsupported("cliInvoke");
  }

  /**
   * Starts long-running guest work after a successful handshake.
   *
   * @param _params - Optional start parameters (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async start(_params?: unknown): Promise<void> {
    throw unsupported("start");
  }

  /**
   * Drains queued plugin → host events.
   *
   * @returns Queued events (shape is kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async pollEvents(): Promise<unknown> {
    throw unsupported("pollEvents");
  }

  /**
   * Scans the operator library through an integration guest.
   *
   * @param _params - Scan scope and account filters.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async scanLibrary(_params: unknown): Promise<void> {
    throw unsupported("scanLibrary");
  }

  /**
   * Syncs listening progress with an external library server.
   *
   * @returns Sync summary (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async syncListening(): Promise<unknown> {
    throw unsupported("syncListening");
  }

  /**
   * Validates an external user identity for portal / OIDC flows.
   *
   * @param _params - Credentials or tokens from the connect portal.
   * @returns Authentication result (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async authenticateUser(_params: unknown): Promise<unknown> {
    throw unsupported("authenticateUser");
  }

  /**
   * Performs a one-shot store login (password / token flows).
   *
   * @param _params - Store credentials and account labeling.
   * @returns Login result (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async login(_params: unknown): Promise<unknown> {
    throw unsupported("login");
  }

  /**
   * Starts an interactive OAuth (or similar) login.
   *
   * @param _params - Marketplace / locale hints for the authorize URL.
   * @returns Login-start result including authorize URL when applicable.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async loginStart(_params: unknown): Promise<unknown> {
    throw unsupported("loginStart");
  }

  /**
   * Completes an interactive login started by {@link BookclerkPlugin.loginStart}.
   *
   * @param _params - Callback payload / authorization code.
   * @returns Login result (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async loginComplete(_params: unknown): Promise<unknown> {
    throw unsupported("loginComplete");
  }

  /**
   * Updates stored credentials without a full login round-trip.
   *
   * @param _params - Account id and replacement secret material.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async credentialsUpdate(_params: unknown): Promise<void> {
    throw unsupported("credentialsUpdate");
  }

  /**
   * Scans owned titles from a source storefront.
   *
   * @param _params - Account filters and pagination options.
   * @returns Scan summary (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async scan(_params: unknown): Promise<unknown> {
    throw unsupported("scan");
  }

  /**
   * Downloads (and decrypts, when applicable) one title to a fetch directory.
   *
   * @param _params - Title id / ASIN / ISBN and destination hints.
   * @returns Fetch result describing acquired files.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async fetchTitle(_params: unknown): Promise<unknown> {
    throw unsupported("fetchTitle");
  }

  /**
   * Searches the storefront catalog.
   *
   * @param _params - Query string and optional filters.
   * @returns Catalog hits.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async searchCatalog(_params: unknown): Promise<unknown> {
    throw unsupported("searchCatalog");
  }

  /**
   * Expands related catalog candidates for a seed title.
   *
   * @param _params - Seed identifiers and expansion limits.
   * @returns Related catalog hits.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async expandCandidates(_params: unknown): Promise<unknown> {
    throw unsupported("expandCandidates");
  }

  /**
   * Returns a purchase / wishlist hint for a catalog title when available.
   *
   * @param _params - Title identifier in the storefront namespace.
   * @returns Purchase hint or nullish when unavailable.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async purchaseHint(_params: unknown): Promise<unknown> {
    throw unsupported("purchaseHint");
  }

  /**
   * Lists current deals / sales from the storefront.
   *
   * @param _params - Pagination and marketplace filters.
   * @returns Catalog hits for deals.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async listDeals(_params: unknown): Promise<unknown> {
    throw unsupported("listDeals");
  }

  /**
   * Lists connected source accounts known to this guest.
   *
   * @param _params - Optional account-id filter.
   * @returns Account descriptors.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async listAccounts(_params: unknown): Promise<unknown> {
    throw unsupported("listAccounts");
  }

  /**
   * Fetches rich catalog detail for one title.
   *
   * @param _params - Storefront title identifier.
   * @returns Catalog detail or nullish when missing.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async catalogDetail(_params: unknown): Promise<unknown> {
    throw unsupported("catalogDetail");
  }

  /**
   * Writes bytes to a destination object key.
   *
   * @param _params - Object key and inline payload.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async put(_params: unknown): Promise<void> {
    throw unsupported("put");
  }

  /**
   * Uploads a local file to a destination object key.
   *
   * @param _params - Object key and absolute source path.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async putFile(_params: unknown): Promise<void> {
    throw unsupported("putFile");
  }

  /**
   * Reads an object from the destination.
   *
   * @param _params - Object key and optional byte range.
   * @returns Object bytes or metadata (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async get(_params: unknown): Promise<unknown> {
    throw unsupported("get");
  }

  /**
   * Tests whether a destination object key exists.
   *
   * @param _params - Object key.
   * @returns True when the object exists.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async exists(_params: unknown): Promise<boolean> {
    throw unsupported("exists");
  }

  /**
   * Lists objects under a destination prefix.
   *
   * @param _params - Prefix and pagination options.
   * @returns Listing result (kind-specific).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async list(_params: unknown): Promise<unknown> {
    throw unsupported("list");
  }

  /**
   * Probes destination connectivity / credentials.
   *
   * @param _params - Probe options (kind-specific).
   * @returns Probe result.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async probe(_params: unknown): Promise<unknown> {
    throw unsupported("probe");
  }

  /**
   * Copies an object within the destination.
   *
   * @param _params - Source and destination keys.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async copy(_params: unknown): Promise<void> {
    throw unsupported("copy");
  }

  /**
   * Deletes an object from the destination.
   *
   * @param _params - Object key.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async delete(_params: unknown): Promise<void> {
    throw unsupported("delete");
  }

  /**
   * Updates mtime / metadata for a destination object without rewriting bytes.
   *
   * @param _params - Object key and touch options.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async touchFile(_params: unknown): Promise<void> {
    throw unsupported("touchFile");
  }

  /**
   * Opens a database session for a database-kind guest.
   *
   * @param _params - Connection / DSN options.
   * @returns Connection handle or session descriptor.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbConnect(_params: unknown): Promise<unknown> {
    throw unsupported("dbConnect");
  }

  /**
   * Pings an open database session.
   *
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbPing(): Promise<void> {
    throw unsupported("dbPing");
  }

  /**
   * Runs a read query against the database guest.
   *
   * @param _params - Statement and bind parameters.
   * @returns Query rows / result set.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbQuery(_params: unknown): Promise<unknown> {
    throw unsupported("dbQuery");
  }

  /**
   * Runs a write / execute statement against the database guest.
   *
   * @param _params - Statement and bind parameters.
   * @returns Execute result (rows affected, etc.).
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbExecute(_params: unknown): Promise<unknown> {
    throw unsupported("dbExecute");
  }

  /**
   * Begins a database transaction (or nested savepoint).
   *
   * @param _params - Optional parent transaction id.
   * @returns `{ txnId }`.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbBegin(_params: unknown): Promise<unknown> {
    throw unsupported("dbBegin");
  }

  /**
   * Commits a guest transaction.
   *
   * @param _params - `{ txnId }`.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbCommit(_params: unknown): Promise<void> {
    throw unsupported("dbCommit");
  }

  /**
   * Rolls back a guest transaction.
   *
   * @param _params - `{ txnId }`.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbRollback(_params: unknown): Promise<void> {
    throw unsupported("dbRollback");
  }

  /**
   * Runs a named atomic library operation as one guest SQL transaction.
   *
   * @param _params - Tagged operation.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async dbAtomic(_params: unknown): Promise<unknown> {
    throw unsupported("dbAtomic");
  }

  /**
   * Fallback dispatcher for unknown wire method names.
   *
   * @param _method - Wire method name.
   * @param _params - Raw params object from the host.
   * @returns Method-specific result.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async callRaw(_method: string, _params: unknown): Promise<unknown> {
    throw unsupported("callRaw");
  }
}

/**
 * Native guest runner. Newline JSON `serve` was removed; JS/TS authors export a
 * workerd {@link BookclerkPlugin}. Native guests use Rust `serve` / `PluginRoot`.
 */
export class BookclerkPluginGuest {
  /**
   * Throws: newline JSON native serve is no longer part of the product ABI.
   *
   * @param _plugin - Ignored.
   */
  static async serve(_plugin: BookclerkPluginLike): Promise<void> {
    throw new Error(
      "newline JSON native serve was removed; export a workerd BookclerkPlugin or use Rust serve()/PluginRoot",
    );
  }
}

function unsupported(method: string): Error {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported" as const,
  });
}
