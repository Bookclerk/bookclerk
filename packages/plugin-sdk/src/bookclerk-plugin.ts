import "./cloudflare-workers.d.ts";
import { WorkerEntrypoint } from "cloudflare:workers";
import type {
  BookclerkEnv,
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
 * Branded guest base — extends `WorkerEntrypoint` with Bookclerk env + contract hooks.
 *
 * Authors subclass this type (never bare `WorkerEntrypoint`) and override the
 * methods their `capabilities.methods` declare. The method surface matches the
 * native {@link BookclerkPlugin} in `@bookclerk/plugin-sdk/native`.
 *
 * Required: {@link BookclerkPlugin.handshake}.
 *
 * Defaults: {@link BookclerkPlugin.shutdown}, {@link BookclerkPlugin.health},
 * {@link BookclerkPlugin.diagnose}.
 *
 * Optional overrides (omit or leave unimplemented until needed):
 * `onEvent`, `cliDescribe`, `cliInvoke`, `start`, `pollEvents`, kind-specific
 * methods (`scan`, `fetchTitle`, storage/db ops, …).
 *
 * @example
 * ```ts
 * export default class MyPlugin extends BookclerkPlugin {
 *   async handshake(params: HandshakeParams): Promise<HandshakeResult> {
 *     return {
 *       apiVersion: 1,
 *       id: "echo",
 *       kind: "source",
 *       capabilities: ["handshake", "health"],
 *     };
 *   }
 * }
 * ```
 */
export abstract class BookclerkPlugin extends WorkerEntrypoint<BookclerkEnv> {
  /** Bookclerk host bindings (`HOST`, `CONFIG`, `PLUGIN_KV`, …). */
  declare readonly env: BookclerkEnv;

  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * Required by workerd when the entrypoint is not HTTP-facing.
   *
   * @returns Always a 404 empty response.
   */
  async fetch(): Promise<Response> {
    return new Response(null, { status: 404 });
  }

  /**
   * Negotiates ABI version and advertises plugin identity to the host.
   *
   * @param params - Host-provided handshake inputs (API version, config).
   * @returns Guest identity, kind, and negotiated capabilities.
   */
  abstract handshake(params: HandshakeParams): Promise<HandshakeResult>;

  /**
   * Releases guest resources before the isolate exits.
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
   * Handles a host → plugin push event (`onEvent`).
   *
   * Override when advertising the `onEvent` capability.
   *
   * @param _event - Event envelope from the host.
   * @throws {Error} With `code: "unsupported"` unless overridden.
   */
  async onEvent(_event: HostToPluginEvent): Promise<void> {
    throw unsupported("onEvent");
  }

  /**
   * Describes the guest CLI surface (mirrors handshake `cli` when present).
   *
   * @returns CLI schema; default has no commands.
   */
  async cliDescribe(): Promise<CliSchema> {
    return { commands: [] };
  }

  /**
   * Runs a plugin CLI command (`cliInvoke`).
   *
   * Override when advertising the `cli` capability.
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
}

function unsupported(method: string): Error {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported" as const,
  });
}
