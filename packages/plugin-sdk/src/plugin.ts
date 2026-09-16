/**
 * TypeScript class ABI for `apiVersion` 2 (object-capability Workers RPC).
 *
 * Authors subclass {@link BookclerkPlugin} and return {@link Destination} /
 * {@link Source} / {@link JobHandler} RpcTargets. Byte payloads move as
 * `ReadableStream` — never as base64 scalars, `handleId`, or `writeChunk`.
 */

import "./cloudflare-workers.d.ts";
import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";
import { MAX_LIST_PAGE, MAX_SCALAR_BYTES } from "./abi.js";
import { createDatabaseBinding, decodeExecuteResultReply, encodeExecuteRequest } from "./db-execute.js";
import type { ExecuteReply, ExecuteRequest } from "./db-execute.js";
import { requirePluginMigrationRegistration } from "./plugin-migrations.js";
import type {
  AuthenticateUserParams,
  CatalogDetailParams,
  CatalogHit,
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  ContentSourceContext,
  DatabaseContext,
  DestinationContext,
  ExpandCandidatesParams,
  ExternalUser,
  FetchTitleParams,
  HealthOk,
  IntegrationContext,
  ListDealsParams,
  ListeningProgress,
  LoginCompleteParams,
  LoginParams,
  LoginResult,
  LoginStartResult,
  PlainFetch,
  PluginDescribe as WirePluginDescribe,
  PurchaseHint,
  PurchaseHintParams,
  ScanLibraryParams,
  ScanParams,
  ScanSummary,
  SearchCatalogParams,
  SourceAccount,
  SourceContext,
  WorkerContext,
} from "./generated.js";

// Product constants come from the generated `abi.ts` projection of
// `schema/plugin.capnp` — re-exported here for guest convenience.
export {
  FEATURE_SCALAR_LIMITS,
  FEATURE_STORAGE_COPY,
  FEATURE_STREAMS,
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  PRODUCT_API_VERSION,
} from "./abi.js";
export { requirePluginMigrationRegistration } from "./plugin-migrations.js";

// Typed factory contexts and method payloads are the generated projections of
// `schema/plugin.capnp`; re-exported so guests can import them beside the
// role classes.
export type {
  CliInvokeParams,
  CliInvokeResult,
  CliSchema,
  ContentSourceContext,
  DatabaseContext,
  DestinationContext,
  ExtensibleConfig,
  HealthOk,
  IntegrationContext,
  ScalarLimits,
  SourceContext,
  WorkerContext,
} from "./generated.js";

/** Describe fields every guest must advertise. */
type RequiredDescribe = Pick<
  WirePluginDescribe,
  "apiVersion" | "id" | "kind" | "rpcFeatures" | "scalarLimits"
>;

/**
 * Guest identity returned by `BookclerkPlugin.describe`.
 *
 * The wire struct is the generated `PluginDescribe` in `generated.ts`; every
 * field beyond `apiVersion`, `id`, `kind`, `rpcFeatures`, and `scalarLimits`
 * defaults to its zero value (empty list, `0`, empty `brand`, empty `cli`)
 * when omitted.
 */
export interface PluginDescribe
  extends RequiredDescribe,
    Partial<Omit<WirePluginDescribe, keyof RequiredDescribe>> {}

/** Bookclerk-as-IdP relying-party template declared by a guest. */
export interface OidcClientTemplate {
  clientId: string;
  displayName?: string;
  callbackPath: string;
  publicClient?: boolean;
  defaultScopes?: string[];
  issueRefreshToken?: boolean;
  originConfigKey: string;
}

/** One already-separated BookclerkSQL operation in a plugin-owned migration. */
export type PluginMigrationOp = { schema: string } | { data: string };

/** One plugin-owned migration application. `id` is opaque plugin-chosen identity. */
export interface PluginMigration {
  id: string;
  operations: PluginMigrationOp[];
}

/**
 * Schema DDL convenience for {@link PluginMigration.operations}.
 *
 * @param sql - Already-separated BookclerkSQL schema statement.
 * @returns Schema operation tagged for host registration.
 */
export function schemaMigrationOp(sql: string): PluginMigrationOp {
  return { schema: sql };
}

/**
 * Data DML convenience for {@link PluginMigration.operations}.
 *
 * @param sql - Already-separated BookclerkSQL data statement.
 * @returns Data operation tagged for host registration.
 */
export function dataMigrationOp(sql: string): PluginMigrationOp {
  return { data: sql };
}

/** Bounded, versioned checkpoint. */
export interface JobCheckpoint {
  schemaVersion: number;
  json?: string;
}

/**
 * Versioned durable command envelope (not a domain event).
 *
 * Idempotency keys are scoped to `(account, plugin, commandType)` until a
 * terminal fenced outcome is committed. `deadlineUnixMs` is a guest hint; the
 * host fence/lease is authoritative.
 */
export interface JobInvocation {
  payloadSchemaVersion: number;
  invocationId: string;
  commandType: string;
  payloadJson?: string;
  idempotencyKey: string;
  attempt: number;
  correlationId?: string;
  causationId?: string;
  deadlineUnixMs: number;
  checkpoint?: JobCheckpoint;
}

/** Handler completion. Suspension is durable only after a fenced commit. */
export type JobOutcome =
  | { kind: "completed"; message?: string; bytesCopied?: number }
  | { kind: "retryable"; message?: string; retryAfterUnixMs?: number }
  | { kind: "rejected"; message?: string }
  | { kind: "cancelled"; message?: string }
  | {
      kind: "suspended";
      checkpoint: JobCheckpoint;
      wakeAtUnixMs: number;
    };

/** Versioned domain event (not a job). */
export interface DomainEvent {
  eventId: string;
  eventType: string;
  schemaVersion: number;
  occurredAtUnixMs: number;
  accountId?: string;
  /** Producer plugin id; empty/omitted when unknown. */
  source?: string;
  correlationId?: string;
  causationId?: string;
  deduplicationKey?: string;
  deliveryAttempt: number;
  payload?: Uint8Array;
  /** Checkpoint JSON from a prior `suspended` result. */
  checkpointJson?: string;
  checkpointSchemaVersion?: number;
  invocationSequence?: number;
  /** True when this invocation continues a prior `suspended` result. */
  resumePending?: boolean;
}

/** Result of {@link Integration.onEvent}. */
export type EventResult =
  | { kind: "ack" }
  | { kind: "retry"; retryAtUnixMs: number; reason?: string }
  | { kind: "reject"; reason?: string }
  | { kind: "deadLetter"; reason?: string }
  | {
      kind: "suspended";
      checkpointJson?: string;
      checkpointSchemaVersion?: number;
      wakeAtUnixMs: number;
      /** Event type that can wake this sleep; empty = timestamp-only. */
      wakeOnEventType?: string;
      /** Host-owned payload object filter JSON; empty = type only. */
      wakeOnFilterJson?: string;
    };

/** Author-visible granted bindings. Adapter-private tokens are not present. */
export interface GrantedBindings {
  HTTP?: BookclerkPluginEnv["HTTP"];
  STORAGE?: unknown;
  SECRETS?: unknown;
  OAUTH?: unknown;
  /** Host-mediated typed SQL execute surface when a database grant is present. */
  DATABASE?: import("./db-execute.js").DatabaseBinding;
}

/** Invocation identity (never a PID, RpcTarget, or adapter map id). */
export interface InvocationContext {
  invocationId?: string;
  grantRevision?: string;
  role?: string;
  accountId?: string;
}

/**
 * Typed native operations. The host executor chooses the executable and
 * sandbox; plugin input cannot weaken them. `PLUGIN_BACKEND` is private
 * workerd config and is never present on this object.
 */
export interface NativeBinding {
  describe(): Promise<PluginDescribe>;
  destination(ctx: DestinationContext): Destination | Promise<Destination>;
  source(ctx: SourceContext): Source | Promise<Source>;
  worker(ctx: WorkerContext): JobHandler | Promise<JobHandler>;
  contentSource?(ctx: ContentSourceContext): ContentSource | Promise<ContentSource>;
  integration?(ctx: IntegrationContext): Integration | Promise<Integration>;
  database?(ctx: DatabaseContext): Database | Promise<Database>;
}

/** Frozen per-invocation context constructed by the trusted adapter. */
export interface BookclerkContext {
  bindings: GrantedBindings;
  native?: NativeBinding;
  invocation: InvocationContext;
}

const KNOWN_ERROR_CODES = new Set([
  "invalid_params",
  "unauthorized",
  "forbidden",
  "not_found",
  "unavailable",
  "unsupported",
  "internal",
  "payload_too_large",
  "deadline_exceeded",
  "invalid_cursor",
  "cancelled",
  "conflict",
]);

/** Thrown by the SDK when a wire union carries `err`. Unknown codes are kept. */
export class PluginError extends Error {
  readonly code: string;
  readonly wireCode: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
  }

  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @returns Typed plugin error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}

/** Author-visible bindings. Adapter-private tokens are not present. */
export interface BookclerkPluginEnv {
  /**
   * Host-approved HTTP. Absent when the plugin has no network grant.
   */
  HTTP?: {
    /**
     * Fetch through the host egress policy.
     *
     * @param input - Request URL or Request.
     * @param init - Optional fetch init.
     * @returns Host response.
     */
    fetch: typeof fetch;
  };
  /** Opaque storage binding when the host injects one. */
  STORAGE?: unknown;
  /** Opaque secrets binding when the host injects one. */
  SECRETS?: unknown;
  /** Opaque OAuth binding when the host injects one. */
  OAUTH?: unknown;
  /** Host-mediated typed SQL execute surface when a database grant is present. */
  DATABASE?: import("./db-execute.js").DatabaseBinding;
}

/** First-party wrapper env. Authors never see this type on their class. */
export interface AdapterEnv {
  /** Author plugin isolate. Wrapper-only. */
  PLUGIN?: BookclerkPlugin;
  /** Native jail / workerd backend handle. Wrapper-only. */
  PLUGIN_BACKEND?: unknown;
  /**
   * Per-invocation grant reverse channel. Wrapper-only; stripped from author env.
   */
  GRANTED?: {
    /**
     * Call the host grant broker.
     *
     * @param input - Request URL.
     * @param init - Optional fetch init.
     * @returns Host response.
     */
    fetch: (input: string, init?: RequestInit) => Promise<Response>;
  };
  /** Isolate-to-host notify token. Wrapper-only; stripped from author env. */
  BRIDGE_TOKEN?: string;
}

/** Object listing entry. */
export interface ObjectInfo {
  key: string;
  size: number;
}

/** Metadata without a body. */
export interface ObjectMetadata {
  key: string;
  size: number;
  contentType?: string;
  etag?: string;
  sha256?: Uint8Array;
}

/** Paginated list request. */
export interface ListOptions {
  prefix?: string;
  cursor?: string;
  limit?: number;
}

/** One page of keys. */
export interface ListPage {
  objects: ObjectInfo[];
  nextCursor?: string;
}

/** Optional byte range for a streamed read. */
export interface ByteRange {
  offset: number;
  length?: number;
}

/** Read options for {@link Destination.get}. */
export interface ReadOptions {
  range?: ByteRange;
}

/** Write options for {@link Destination.put}. */
export interface WriteOptions {
  contentType?: string;
  contentLength?: number;
  sha256?: Uint8Array;
  commitToken?: string;
  stageOnly?: boolean;
}

/** Streamed read result. `body` is a transferred ReadableStream. */
export interface ReadResult {
  meta: ObjectMetadata;
  body: ReadableStream<Uint8Array>;
}

/** Result of a streamed put. */
export interface PutResult {
  key: string;
  bytesWritten: number;
  etag?: string;
  sha256?: Uint8Array;
}

/** Result of a server-side copy. */
export interface CopyResult {
  bytesCopied: number;
}

/** Granted stubs for one {@link JobHandler.handle} invocation. */
export interface JobContext {
  input: Source;
  output: Destination;
  progress: ProgressSink;
  /**
   * Unused in production. Jobs never inject the host library as guest SQL;
   * durable plugin state uses {@link JobContext.databases}.
   */
  database?: import("./db-execute.js").DatabaseBinding;
  /**
   * Named plugin-owned database bindings (Workers-style) declared in
   * `plugin.toml` `capabilities.bindings.databases` and approved by the
   * operator. Each binding is an isolated database — separate from the
   * Bookclerk library and every other plugin — with full DML plus bounded
   * idempotent DDL (`CREATE`/`DROP` `TABLE`/`INDEX` with `IF [NOT] EXISTS`).
   * `ALTER` and `CREATE TABLE AS` are refused.
   */
  databases?: Map<string, import("./db-execute.js").DatabaseBinding>;
  signal?: AbortSignal;
}

/**
 * Destination capability (storage). The runtime stub *is* the capability.
 */
export class Destination extends RpcTarget {
  /**
   * Metadata without a body; `null` when the key is missing.
   *
   * @param _key - Object key.
   * @returns Metadata or `null` when the key is missing.
   */
  head(_key: string): Promise<ObjectMetadata | null> {
    return Promise.reject(unsupported("head"));
  }

  /**
   * One page of keys under `options.prefix`.
   *
   * @param _options - Prefix, cursor, and limit.
   * @returns One page of object keys.
   */
  list(_options: ListOptions): Promise<ListPage> {
    return Promise.reject(unsupported("list"));
  }

  /**
   * Streamed read. The body is a transferred stream, not a scalar.
   *
   * @param _key - Object key.
   * @param _options - Optional byte range.
   * @returns Metadata plus a transferred body stream.
   */
  get(_key: string, _options?: ReadOptions): Promise<ReadResult> {
    return Promise.reject(unsupported("get"));
  }

  /**
   * Streamed write. `body` ownership is transferred to the destination.
   *
   * @param _key - Object key.
   * @param _body - Byte stream.
   * @param _options - Optional content type / length.
   * @returns Bytes written and optional etag / sha256.
   */
  put(
    _key: string,
    _body: ReadableStream<Uint8Array>,
    _options?: WriteOptions,
  ): Promise<PutResult> {
    return Promise.reject(unsupported("put"));
  }

  /**
   * Server-side copy when the backend supports it.
   *
   * @param _from - Source key.
   * @param _to - Destination key.
   * @returns Bytes copied.
   */
  copy?(_from: string, _to: string): Promise<CopyResult> {
    return Promise.reject(unsupported("copy"));
  }

  /**
   * Delete a key (no-op if missing).
   *
   * @param _key - Object key.
   * @returns Resolves when the delete is complete.
   */
  delete(_key: string): Promise<void> {
    return Promise.reject(unsupported("delete"));
  }

  /**
   * Finalize a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Idempotency / commit token.
   * @returns Published object metadata.
   */
  commit(_key: string, _commitToken: string): Promise<PutResult> {
    return Promise.reject(unsupported("commit"));
  }

  /**
   * Abort a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Staging token to discard.
   * @returns Rejects with typed `unsupported` unless overridden.
   */
  abortStage(_key: string, _commitToken: string): Promise<void> {
    return Promise.reject(unsupported("abortStage"));
  }
}

/**
 * Source capability that can open a named object as a stream.
 */
export class Source extends RpcTarget {
  /**
   * Opens `key` for streamed reading.
   *
   * @param _key - Object key.
   * @returns Metadata plus a transferred body stream.
   */
  open(_key: string): Promise<ReadResult> {
    return Promise.reject(unsupported("open"));
  }
}

/**
 * Progress reports for a job invocation (never carries media).
 */
export class ProgressSink extends RpcTarget {
  /**
   * Reports `percent` in `0..=100` and an operator-facing `message`.
   *
   * @param _percent - Completion percent.
   * @param _message - Operator-facing status.
   * @returns Resolves when the host records progress.
   */
  report(_percent: number, _message: string): Promise<void> {
    return Promise.reject(unsupported("report"));
  }
}

/**
 * Plugin worker that handles one durable job invocation.
 */
export class JobHandler extends RpcTarget {
  /**
   * Runs `invocation` using granted capabilities until completion or cancel.
   *
   * @param _invocation - Durable command envelope (no media bytes).
   * @param _context - Granted source, destination, and progress stubs.
   * @returns Job outcome.
   */
  handle(_invocation: JobInvocation, _context: JobContext): Promise<JobOutcome> {
    return Promise.reject(unsupported("handle"));
  }
}

/**
 * Storefront content source (not byte {@link Source}).
 *
 * Every method takes and returns the typed ABI structs generated from
 * `schema/plugin.capnp`; the host never sees JSON text for these payloads.
 */
export class ContentSource extends RpcTarget {
  /**
   * Password or one-shot OAuth login. The host seals
   * {@link LoginResult.credentials} into `encrypted_secrets`.
   *
   * @param _params - Login parameters.
   * @returns Account identity plus opaque credentials.
   */
  login(_params: LoginParams): Promise<LoginResult> {
    return Promise.reject(unsupported("login"));
  }
  /**
   * Library scan; the host upserts {@link ScanSummary.books}.
   *
   * @param _params - Scan parameters (accounts + credentials).
   * @returns Books discovered plus per-account counters.
   */
  scan(_params: ScanParams): Promise<ScanSummary> {
    return Promise.reject(unsupported("scan"));
  }
  /**
   * Fetches one title into `params.cacheDir` and returns plain media paths.
   *
   * @param _params - Title identifiers, credentials, fetch options.
   * @returns Plain media parts plus metadata.
   */
  fetchTitle(_params: FetchTitleParams): Promise<PlainFetch> {
    return Promise.reject(unsupported("fetchTitle"));
  }
  /**
   * Accounts the guest knows about.
   *
   * @returns Account list.
   */
  listAccounts(): Promise<SourceAccount[]> {
    return Promise.reject(unsupported("listAccounts"));
  }
  /**
   * Begins an interactive OAuth login.
   *
   * @param _params - Login parameters.
   * @returns Continuation for {@link ContentSource.loginComplete}.
   */
  loginStart(_params: LoginParams): Promise<LoginStartResult> {
    return Promise.reject(unsupported("loginStart"));
  }
  /**
   * Finishes a login started by {@link ContentSource.loginStart}.
   *
   * @param _params - Continuation plus user input.
   * @returns Account identity plus opaque credentials.
   */
  loginComplete(_params: LoginCompleteParams): Promise<LoginResult> {
    return Promise.reject(unsupported("loginComplete"));
  }
  /**
   * Free-text storefront catalog search.
   *
   * @param _params - Query parameters.
   * @returns Catalog hits.
   */
  searchCatalog(_params: SearchCatalogParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("searchCatalog"));
  }
  /**
   * Related-title expansion from a seed title.
   *
   * @param _params - Seed title plus limits.
   * @returns Candidate hits.
   */
  expandCandidates(_params: ExpandCandidatesParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("expandCandidates"));
  }
  /**
   * Purchase link / price hint; `null` when the title is unknown.
   *
   * @param _params - Title identifiers.
   * @returns Hint or `null`.
   */
  purchaseHint(_params: PurchaseHintParams): Promise<PurchaseHint | null> {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Current storefront deals.
   *
   * @param _params - Optional filters.
   * @returns Deal hits.
   */
  listDeals(_params: ListDealsParams): Promise<CatalogHit[]> {
    return Promise.reject(unsupported("listDeals"));
  }
  /**
   * Full catalog record for one product; `null` when unknown.
   *
   * @param _params - Product identifiers.
   * @returns Catalog hit or `null`.
   */
  catalogDetail(_params: CatalogDetailParams): Promise<CatalogHit | null> {
    return Promise.reject(unsupported("catalogDetail"));
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose(): Promise<string[]> {
    return Promise.resolve([]);
  }
  /**
   * Reports whether the storefront session is usable.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
}

/** Integration role. {@link Integration.onEvent} is not a generic job container. */
export class Integration extends RpcTarget {
  /**
   * Reports whether the integration session is usable.
   *
   * @returns Health flag plus detail.
   */
  health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose(): Promise<string[]> {
    return Promise.resolve([]);
  }
  /**
   * Handles one versioned {@link DomainEvent}.
   *
   * @param _event - Delivered event envelope.
   * @returns Ack, retry, reject, dead-letter, or suspended.
   */
  onEvent(_event: DomainEvent): Promise<EventResult> {
    return Promise.reject(unsupported("onEvent"));
  }
  /**
   * Starts long-running integration work for this invocation.
   *
   * @returns Resolves when the integration is running.
   */
  start(): Promise<void> {
    return Promise.resolve();
  }
  /**
   * Stops long-running integration work for this invocation.
   *
   * @returns Resolves when the integration has stopped.
   */
  stop(): Promise<void> {
    return Promise.resolve();
  }
  /**
   * Re-syncs the remote library.
   *
   * @param _params - Scan scope.
   * @returns Resolves when the scan has been accepted.
   */
  scanLibrary(_params: ScanLibraryParams): Promise<void> {
    return Promise.reject(unsupported("scanLibrary"));
  }
  /**
   * Push / pull listening progress; the host upserts the rows.
   *
   * @returns Progress rows.
   */
  syncListening(): Promise<ListeningProgress[]> {
    return Promise.reject(unsupported("syncListening"));
  }
  /**
   * Verifies remote credentials on behalf of the host.
   *
   * @param _params - Username / password pair.
   * @returns External user identity.
   */
  authenticateUser(_params: AuthenticateUserParams): Promise<ExternalUser> {
    return Promise.reject(unsupported("authenticateUser"));
  }
  /**
   * Drains external users observed since the last poll.
   *
   * @returns Newly observed users.
   */
  pollEvents(): Promise<ExternalUser[]> {
    return Promise.reject(unsupported("pollEvents"));
  }
}

/** Database factory. Sessions cannot survive suspension. */
export class Database extends RpcTarget {
  openSession(): Promise<AdapterDatabaseSession> {
    return Promise.reject(unsupported("openSession"));
  }
}

/** Host ↔ database adapter session (`capabilities` + typed `execute`). */
export class AdapterDatabaseSession extends RpcTarget {
  /**
   * Typed SQL-contract advertisement.
   *
   * @returns Guest `DbCapabilities`.
   */
  capabilities(): Promise<unknown> {
    return Promise.reject(unsupported("capabilities"));
  }
  /**
   * Typed atomic batch (`ExecuteRequest` → `ExecuteReply`).
   *
   * @param _request - Cap'n `ExecuteRequest` (structured Workers RPC object).
   * @returns `ExecuteReply`.
   */
  execute(_request: import("./db-execute.js").ExecuteRequest): Promise<import("./db-execute.js").ExecuteReply> {
    return Promise.reject(unsupported("execute"));
  }
  /** Close the adapter session.
   *
   * @returns Resolves when the session is closed.
   */
  close(): Promise<void> {
    return Promise.resolve();
  }
}

/** Host-granted SQL transport for job plugin authors (no `capabilities`). */
export class GuestDatabase extends RpcTarget {
  /**
   * Host-mediated typed batch (`ExecuteRequest` → `ExecuteReply`).
   *
   * @param _request - Cap'n `ExecuteRequest`.
   * @returns `ExecuteReply`.
   */
  execute(_request: import("./db-execute.js").ExecuteRequest): Promise<import("./db-execute.js").ExecuteReply> {
    return Promise.reject(unsupported("execute"));
  }
  /** Close the granted database handle.
   *
   * @returns Resolves when the grant is released.
   */
  close(): Promise<void> {
    return Promise.resolve();
  }
}

/**
 * Rejects a PUT body that does not match a declared `Content-Length`.
 *
 * @param body - Incoming byte stream.
 * @param expected - Declared length; omitted streams pass through.
 * @returns The original stream, or a wrapping stream that errors on mismatch.
 */
function exactLengthBody(
  body: ReadableStream<Uint8Array> | null | undefined,
  expected: number | undefined,
): ReadableStream<Uint8Array> | null | undefined {
  if (body == null || expected == null || !Number.isFinite(expected)) {
    return body;
  }
  const reader = body.getReader();
  let n = 0;
  return new ReadableStream({
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) {
        if (n !== expected) {
          controller.error(
            PluginError.fromWire("invalid_params", `content-length ${expected} got ${n}`),
          );
          return;
        }
        controller.close();
        return;
      }
      n += value.byteLength;
      if (n > expected) {
        controller.error(
          PluginError.fromWire("payload_too_large", `body exceeded Content-Length ${expected}`),
        );
        return;
      }
      controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

type GrantedFetcher = NonNullable<AdapterEnv["GRANTED"]>;

/** Adapter-isolate source stub; methods run where `GRANTED` is bound. */
class GrantedSource extends Source {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async open(key: string) {
    const resp = await this.#granted.fetch(
      `http://granted/open?key=${encodeURIComponent(key)}`,
      { headers: this.#auth, signal: this.#signal },
    );
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return {
      meta: {
        key: resp.headers.get("x-bookclerk-key") || key,
        size: Number(resp.headers.get("x-bookclerk-size") || "0"),
        contentType: resp.headers.get("x-bookclerk-content-type") || undefined,
        etag: resp.headers.get("x-bookclerk-etag") || undefined,
      },
      body: resp.body as ReadableStream<Uint8Array>,
    };
  }
}

/** Adapter-isolate destination stub; methods run where `GRANTED` is bound. */
class GrantedDestination extends Destination {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async put(key: string, body: ReadableStream<Uint8Array>, options?: WriteOptions) {
    const headers: Record<string, string> = { ...this.#auth };
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) {
      headers["content-length"] = String(options.contentLength);
    }
    const resp = await this.#granted.fetch(
      `http://granted/put?key=${encodeURIComponent(key)}`,
      { method: "PUT", headers, body, signal: this.#signal },
    );
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return (await resp.json()) as PutResult;
  }
}

/** Adapter-isolate progress stub; methods run where `GRANTED` is bound. */
class GrantedProgress extends ProgressSink {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async report(percent: number, message?: string) {
    await this.#granted.fetch(`http://granted/progress`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify({ percent, message: message || "" }),
      signal: this.#signal,
    });
  }
}

class GrantedDatabaseTransport {
  #granted: GrantedFetcher;
  #auth: Record<string, string>;
  #signal: AbortSignal;

  constructor(granted: GrantedFetcher, auth: Record<string, string>, signal: AbortSignal) {
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }

  async execute(request: ExecuteRequest): Promise<ExecuteReply> {
    const body = encodeExecuteRequest(request);
    const resp = await this.#granted.fetch(`http://granted/db/execute`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/octet-stream" },
      body,
      signal: this.#signal,
    });
    if (!resp.ok) {
      throw PluginError.fromWire("unavailable", `database grant: ${resp.status}`);
    }
    const bytes = new Uint8Array(await resp.arrayBuffer());
    try {
      return decodeExecuteResultReply(bytes);
    } catch (err) {
      if (err && typeof err === "object" && "wireCode" in err && "message" in err) {
        throw PluginError.fromWire(
          String((err as { wireCode: string }).wireCode),
          String((err as { message: string }).message),
        );
      }
      throw err;
    }
  }
}

function grantedContext(
  env: AdapterEnv,
  grantToken: string,
  controller: AbortController,
  databaseTokens?: Record<string, string>,
): JobContext {
  const granted = env.GRANTED;
  if (!granted || typeof grantToken !== "string" || !grantToken) {
    throw PluginError.fromWire("internal", "granted reverse channel missing");
  }
  const auth = { Authorization: `Bearer ${grantToken}` };
  const databases = new Map<string, import("./db-execute.js").DatabaseBinding>();
  for (const [name, token] of Object.entries(databaseTokens ?? {})) {
    if (typeof token !== "string" || !token) continue;
    const bindingAuth = { Authorization: `Bearer ${token}` };
    databases.set(
      name,
      createDatabaseBinding(
        new GrantedDatabaseTransport(granted, bindingAuth, controller.signal),
      ),
    );
  }
  return {
    input: new GrantedSource(granted, auth, controller.signal),
    output: new GrantedDestination(granted, auth, controller.signal),
    progress: new GrantedProgress(granted, auth, controller.signal),
    database: createDatabaseBinding(
      new GrantedDatabaseTransport(granted, auth, controller.signal),
    ),
    databases,
    signal: controller.signal,
  };
}

/**
 * Product `apiVersion` 2 guest base — `describe` / role factories / shutdown.
 *
 * Authors subclass {@link BookclerkPlugin} and export the raw class. The
 * trusted adapter constructs a frozen {@link BookclerkContext}; authors never
 * see `PLUGIN_BACKEND`, `GRANTED`, or `BRIDGE_TOKEN`.
 */
export abstract class BookclerkPlugin extends WorkerEntrypoint<BookclerkPluginEnv> {
  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * @param _request - Incoming HTTP request when the entrypoint is fetch-facing.
   * @returns Always a 404 empty response.
   */
  async fetch(_request?: Request): Promise<Response> {
    return new Response(null, { status: 404 });
  }

  /** Advertises identity, features, roles, and scalar limits. */
  abstract describe(): Promise<PluginDescribe>;

  /**
   * Returns a destination capability for this invocation.
   *
   * @param _context - Granted settings (`config`); no OS paths.
   */
  destination(_context: DestinationContext): Destination | Promise<Destination> {
    throw unsupported("destination");
  }

  /**
   * Returns a source capability for this invocation.
   *
   * @param _context - Granted settings (`config`); no OS paths.
   */
  source(_context: SourceContext): Source | Promise<Source> {
    throw unsupported("source");
  }

  /**
   * Returns a job handler for this invocation.
   *
   * @param _context - Job id plus granted settings (`config`); no OS paths.
   */
  worker(_context: WorkerContext): JobHandler | Promise<JobHandler> {
    throw unsupported("worker");
  }

  /**
   * Returns a storefront content-source capability.
   *
   * @param _context - Granted `[sources.<id>]` settings.
   */
  contentSource(_context: ContentSourceContext): ContentSource | Promise<ContentSource> {
    throw unsupported("contentSource");
  }

  /**
   * Returns an integration capability.
   *
   * @param _context - Granted `[integrations.<id>]` settings.
   */
  integration(_context: IntegrationContext): Integration | Promise<Integration> {
    throw unsupported("integration");
  }

  /**
   * Returns a database factory.
   *
   * @param _context - Adapter bootstrap or host-private connect params.
   */
  database(_context: DatabaseContext): Database | Promise<Database> {
    throw unsupported("database");
  }

  /**
   * Guest CLI schema.
   *
   * @returns Declared commands (`{ commands: [] }` when the guest has no CLI).
   */
  async cliDescribe(): Promise<CliSchema> {
    return { commands: [] };
  }

  /**
   * Invokes a guest CLI command.
   *
   * @param _params - Command name plus named argument values.
   * @returns Exit code, captured output, optional structured payload.
   */
  async cliInvoke(_params: CliInvokeParams): Promise<CliInvokeResult> {
    throw unsupported("cliInvoke");
  }

  /**
   * Plugin-provided OIDC authorization-server client templates.
   *
   * Empty when the guest is not a relying party. The host materializes
   * `oidc_clients` rows; plugins never mint tokens.
   *
   * @returns Templates (`[]` when unused).
   */
  async oidcClients(): Promise<OidcClientTemplate[]> {
    return [];
  }

  /**
   * Complete ordered plugin-owned migration sequence for one named binding.
   *
   * The host calls this at binding initialization, before ordinary execute.
   * `id` is an opaque plugin-chosen identity. Registration order is the
   * forward sequence. Empty means the binding has no plugin-owned migrations.
   *
   * @param _binding - Binding name from `capabilities.bindings.databases`.
   * @returns Ordered migrations (`[]` when unused).
   */
  async databaseMigrations(_binding: string): Promise<PluginMigration[]> {
    return [];
  }

  /** Releases guest resources. */
  async shutdown(): Promise<void> {}
}

/**
 * Dispose a Workers RPC stub. Cloudflare also disposes when the execution
 * context ends; explicit disposal makes ownership deterministic.
 *
 * @param stub - RpcTarget or thenable stub.
 */
async function disposeRpc(stub: unknown): Promise<void> {
  if (stub == null || typeof stub !== "object") return;
  const obj = stub as { [key: symbol]: unknown };
  try {
    const asyncDispose = obj[Symbol.asyncDispose];
    if (typeof asyncDispose === "function") {
      await (asyncDispose as () => Promise<void>).call(stub);
      return;
    }
    const dispose = obj[Symbol.dispose];
    if (typeof dispose === "function") {
      (dispose as () => void).call(stub);
    }
  } catch {
    // disposal is best-effort
  }
}

export function frozenBookclerkContext(
  env: BookclerkPluginEnv,
  invocation: InvocationContext,
): BookclerkContext {
  const bindings: GrantedBindings = {
    HTTP: env.HTTP,
    STORAGE: env.STORAGE,
    SECRETS: env.SECRETS,
    OAUTH: env.OAUTH,
    DATABASE: env.DATABASE,
  };
  const ctx: BookclerkContext = {
    bindings,
    invocation,
  };
  return Object.freeze(ctx);
}

/**
 * Generated adapter isolate: `env.PLUGIN` is the author worker. One envelope
 * per request: create role, invoke, dispose. Authors cannot replace adapter
 * behavior with adapter-internal names.
 *
 * @returns Wrapper entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromBinding() {
  return createInvocationAdapter();
}

/**
 * Native-behind-workerd generated adapter. Forwards through `ctx.native`.
 * `PLUGIN_BACKEND` stays private workerd config.
 *
 * @returns Wrapper entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapPluginFromNative() {
  return createInvocationAdapter();
}

type NativeErrorEnvelope = { error?: { code: string; message: string } };

/**
 * Bounded native-behind-workerd scalar decoder (`MAX_SCALAR_BYTES + 1`).
 *
 * @param resp - Backend HTTP response.
 * @returns Parsed JSON after status and typed error checks.
 */
async function readNativeScalar<T>(resp: Response): Promise<T> {
  const cap = MAX_SCALAR_BYTES + 1;
  const reader = resp.body?.getReader();
  if (!reader) {
    throw PluginError.fromWire("internal", "native scalar response missing body");
  }
  const chunks: Uint8Array[] = [];
  let n = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    n += value.byteLength;
    if (n > cap) {
      throw PluginError.fromWire(
        "payload_too_large",
        `native scalar exceeded ${MAX_SCALAR_BYTES}`,
      );
    }
    chunks.push(value);
  }
  const buf = new Uint8Array(n);
  let off = 0;
  for (const chunk of chunks) {
    buf.set(chunk, off);
    off += chunk.byteLength;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(buf)) as unknown;
  } catch (err) {
    throw PluginError.fromWire("invalid_params", `malformed native scalar JSON: ${String(err)}`);
  }
  if (parsed && typeof parsed === "object" && "error" in parsed) {
    const envelope = parsed as NativeErrorEnvelope;
    if (envelope.error?.code) {
      throw PluginError.fromWire(envelope.error.code, envelope.error.message ?? envelope.error.code);
    }
  }
  if (!resp.ok) {
    throw PluginError.fromWire("internal", `native scalar HTTP ${resp.status}`);
  }
  return parsed as T;
}

function assertListPage(page: ListPage): ListPage {
  const objects = page.objects ?? [];
  if (objects.length > MAX_LIST_PAGE) {
    throw PluginError.fromWire(
      "payload_too_large",
      `list page ${objects.length} exceeds ${MAX_LIST_PAGE}`,
    );
  }
  for (const obj of objects) {
    if (typeof obj.key !== "string" || obj.key.length > MAX_SCALAR_BYTES) {
      throw PluginError.fromWire("payload_too_large", "list object key too large");
    }
  }
  return { objects, nextCursor: page.nextCursor };
}

/**
 * `Uint8Array` → base64 for the native-broker JSON envelope.
 *
 * @param bytes - Raw bytes.
 * @returns Standard (padded) base64 text.
 */
function bytesToBase64(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/**
 * Typed ABI value → native-broker JSON: Cap'n `Data` fields (`Uint8Array`)
 * travel as base64 text; `bookclerk-workerd` decodes with the same convention.
 *
 * @param value - Plain typed struct value.
 * @returns JSON-safe projection.
 */
function toBridgeJson(value: unknown): unknown {
  if (value instanceof Uint8Array) return bytesToBase64(value);
  if (value instanceof ArrayBuffer) return bytesToBase64(new Uint8Array(value));
  if (Array.isArray(value)) return value.map(toBridgeJson);
  if (value === null || typeof value !== "object") return value;
  const out: Record<string, unknown> = {};
  for (const [key, inner] of Object.entries(value as Record<string, unknown>)) {
    if (inner === undefined) continue;
    out[key] = toBridgeJson(inner);
  }
  return out;
}

function contextHeader(ctx: unknown): string {
  return JSON.stringify(toBridgeJson(ctx ?? {}));
}

const EMPTY_CONFIG = { schemaVersion: 0, mediaType: "", payload: new Uint8Array() };

class HttpNativeDest extends Destination {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };
  #ctx: DestinationContext;

  constructor(
    fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ctx: DestinationContext,
  ) {
    super();
    this.#fetcher = fetcher;
    this.#ctx = ctx ?? { config: EMPTY_CONFIG };
  }

  async head(key: string) {
    const resp = await this.#fetcher.fetch("http://backend/destination/head", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ key, context: this.#ctx })),
    });
    const value = await readNativeScalar<{ found?: boolean; meta?: ObjectMetadata }>(resp);
    return value.found ? (value.meta ?? null) : null;
  }

  async list(options: ListOptions) {
    const resp = await this.#fetcher.fetch("http://backend/destination/list", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ options, context: this.#ctx })),
    });
    return assertListPage(await readNativeScalar<ListPage>(resp));
  }

  async get(key: string, options?: ReadOptions) {
    let path = `/destination/get?key=${encodeURIComponent(key)}`;
    if (options?.range) {
      path += `&offset=${options.range.offset}`;
      if (options.range.length != null) path += `&length=${options.range.length}`;
    }
    const resp = await this.#fetcher.fetch(`http://backend${path}`, {
      headers: { "x-bookclerk-context": contextHeader(this.#ctx) },
    });
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return {
      meta: {
        key: resp.headers.get("x-bookclerk-key") || key,
        size: Number(resp.headers.get("x-bookclerk-size") || "0"),
        contentType: resp.headers.get("x-bookclerk-content-type") || undefined,
        etag: resp.headers.get("x-bookclerk-etag") || undefined,
      },
      body: resp.body as ReadableStream<Uint8Array>,
    };
  }

  async put(key: string, body: ReadableStream<Uint8Array>, options?: WriteOptions) {
    const headers: Record<string, string> = {
      "x-bookclerk-context": contextHeader(this.#ctx),
    };
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) headers["content-length"] = String(options.contentLength);
    if (options?.commitToken) headers["x-bookclerk-commit-token"] = options.commitToken;
    if (options?.stageOnly) headers["x-bookclerk-stage-only"] = "1";
    const resp = await this.#fetcher.fetch(
      `http://backend/destination/put?key=${encodeURIComponent(key)}`,
      { method: "PUT", headers, body },
    );
    return readNativeScalar<PutResult>(resp);
  }

  async copy(from: string, to: string) {
    const resp = await this.#fetcher.fetch("http://backend/destination/copy", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ from, to, context: this.#ctx })),
    });
    return readNativeScalar<CopyResult>(resp);
  }

  async delete(key: string) {
    const resp = await this.#fetcher.fetch("http://backend/destination/delete", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ key, context: this.#ctx })),
    });
    await readNativeScalar<unknown>(resp);
  }

  async commit(key: string, commitToken: string) {
    const resp = await this.#fetcher.fetch("http://backend/destination/commit", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ key, commitToken, context: this.#ctx })),
    });
    return readNativeScalar<PutResult>(resp);
  }

  async abortStage(key: string, commitToken: string) {
    const resp = await this.#fetcher.fetch("http://backend/destination/abortStage", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson({ key, commitToken, context: this.#ctx })),
    });
    await readNativeScalar<unknown>(resp);
  }
}

class HttpNativeSource extends Source {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };
  #ctx: SourceContext;

  constructor(
    fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ctx: SourceContext,
  ) {
    super();
    this.#fetcher = fetcher;
    this.#ctx = ctx ?? { config: EMPTY_CONFIG };
  }

  async open(key: string) {
    const resp = await this.#fetcher.fetch(
      `http://backend/source/open?key=${encodeURIComponent(key)}`,
      { headers: { "x-bookclerk-context": contextHeader(this.#ctx) } },
    );
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return {
      meta: {
        key: resp.headers.get("x-bookclerk-key") || key,
        size: Number(resp.headers.get("x-bookclerk-size") || "0"),
        contentType: resp.headers.get("x-bookclerk-content-type") || undefined,
        etag: resp.headers.get("x-bookclerk-etag") || undefined,
      },
      body: resp.body as ReadableStream<Uint8Array>,
    };
  }
}

class HttpNativeIntegration extends Integration {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };
  #ctx: IntegrationContext;

  constructor(
    fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ctx: IntegrationContext,
  ) {
    super();
    this.#fetcher = fetcher;
    this.#ctx = ctx ?? { config: EMPTY_CONFIG };
  }

  async #json<T>(path: string, body: unknown): Promise<T> {
    const resp = await this.#fetcher.fetch(`http://backend${path}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": contextHeader(this.#ctx),
      },
      body: JSON.stringify(toBridgeJson(body)),
    });
    return readNativeScalar<T>(resp);
  }

  health() {
    return this.#json<HealthOk>("/integration/health", { context: this.#ctx });
  }

  async diagnose() {
    const v = await this.#json<string[] | { lines?: string[] }>("/integration/diagnose", {
      context: this.#ctx,
    });
    return Array.isArray(v) ? v : (v?.lines ?? []);
  }

  onEvent(event: DomainEvent) {
    return this.#json<EventResult>("/integration/onEvent", {
      context: this.#ctx,
      event,
    });
  }

  async start() {
    await this.#json<unknown>("/integration/start", { context: this.#ctx });
  }

  async stop() {
    await this.#json<unknown>("/integration/stop", { context: this.#ctx });
  }
}

class HttpNativeRoot {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };

  constructor(fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch }) {
    this.#fetcher = fetcher;
  }

  async describe() {
    const resp = await this.#fetcher.fetch("http://backend/describe", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    return readNativeScalar<PluginDescribe>(resp);
  }

  destination(ctx: DestinationContext) {
    return new HttpNativeDest(this.#fetcher, ctx);
  }

  source(ctx: SourceContext) {
    return new HttpNativeSource(this.#fetcher, ctx);
  }

  integration(ctx: IntegrationContext) {
    return new HttpNativeIntegration(this.#fetcher, ctx);
  }
}

function nativeRoot(env: AdapterEnv): BookclerkPlugin {
  const backend = env.PLUGIN_BACKEND as
    | (BookclerkPlugin & { fetch?: typeof fetch })
    | undefined;
  if (!backend) {
    throw PluginError.fromWire("unavailable", "PLUGIN_BACKEND binding missing");
  }
  if (typeof backend.fetch === "function") {
    return new HttpNativeRoot(
      backend as NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ) as unknown as BookclerkPlugin;
  }
  return backend as BookclerkPlugin;
}

function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint<AdapterEnv> {
    #plugin(): BookclerkPlugin {
      if (this.env.PLUGIN) {
        return this.env.PLUGIN as BookclerkPlugin;
      }
      if (this.env.PLUGIN_BACKEND) {
        return nativeRoot(this.env);
      }
      throw PluginError.fromWire("unavailable", "PLUGIN binding missing");
    }

    async fetch(_request?: Request): Promise<Response> {
      return new Response(null, { status: 404 });
    }

    async describe(): Promise<PluginDescribe> {
      return this.#plugin().describe();
    }

    destination(ctx: DestinationContext): Destination | Promise<Destination> {
      return this.#plugin().destination(ctx);
    }

    source(ctx: SourceContext): Source | Promise<Source> {
      return this.#plugin().source(ctx);
    }

    worker(ctx: WorkerContext): JobHandler | Promise<JobHandler> {
      return this.#plugin().worker(ctx);
    }

    contentSource(ctx: ContentSourceContext): ContentSource | Promise<ContentSource> {
      return this.#plugin().contentSource(ctx);
    }

    integration(ctx: IntegrationContext): Integration | Promise<Integration> {
      return this.#plugin().integration(ctx);
    }

    database(ctx: DatabaseContext): Database | Promise<Database> {
      return this.#plugin().database(ctx);
    }

    async cliDescribe(): Promise<CliSchema> {
      return this.#plugin().cliDescribe();
    }

    async cliInvoke(params: CliInvokeParams): Promise<CliInvokeResult> {
      return this.#plugin().cliInvoke(params);
    }

    async oidcClients(): Promise<OidcClientTemplate[]> {
      const fn = this.#plugin().oidcClients;
      if (typeof fn !== "function") {
        return [];
      }
      const clients = await fn.call(this.#plugin());
      return Array.isArray(clients) ? clients : [];
    }

    async databaseMigrations(binding: string): Promise<PluginMigration[]> {
      const fn = this.#plugin().databaseMigrations;
      if (typeof fn !== "function") {
        return [];
      }
      const migrations = await fn.call(this.#plugin(), binding);
      const list = Array.isArray(migrations) ? migrations : [];
      try {
        return requirePluginMigrationRegistration(list);
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        throw PluginError.fromWire("payload_too_large", message);
      }
    }

    async shutdown(): Promise<void> {
      await this.#plugin().shutdown();
    }

    /**
     * Create destination, invoke `op`, dispose before returning.
     *
     * @param op - Destination method name.
     * @param ctx - Destination factory context.
     * @param args - Method arguments.
     * @param body - Stream body for `put`.
     * @returns Destination method result.
     */
    async invokeDestination(
      op: string,
      ctx: DestinationContext,
      args: Record<string, unknown> = {},
      body?: ReadableStream<Uint8Array>,
    ): Promise<unknown> {
      const dest = await this.#plugin().destination(ctx ?? { config: EMPTY_CONFIG });
      try {
        switch (op) {
          case "head":
            return await dest.head(String(args.key ?? ""));
          case "list":
            return await dest.list((args.options as ListOptions) ?? {});
          case "get":
            return await dest.get(String(args.key ?? ""), args.options as ReadOptions | undefined);
          case "put": {
            if (!body) {
              throw PluginError.fromWire("invalid_params", "put missing body stream");
            }
            const options = args.options as WriteOptions | undefined;
            const bounded = exactLengthBody(body, options?.contentLength);
            return await dest.put(
              String(args.key ?? ""),
              bounded as ReadableStream<Uint8Array>,
              options,
            );
          }
          case "copy":
            if (typeof dest.copy === "function") {
              return await dest.copy(String(args.from ?? ""), String(args.to ?? ""));
            }
            throw PluginError.fromWire("unsupported", "copy not implemented");
          case "delete":
            await dest.delete(String(args.key ?? ""));
            return { ok: true };
          case "commit":
            return await dest.commit(String(args.key ?? ""), String(args.commitToken ?? ""));
          case "abortStage":
            await dest.abortStage(String(args.key ?? ""), String(args.commitToken ?? ""));
            return { ok: true };
          default:
            throw PluginError.fromWire("unsupported", `destination.${op}`);
        }
      } finally {
        await disposeRpc(dest);
      }
    }

    /**
     * Create source, open, dispose before returning.
     *
     * @param ctx - Source factory context.
     * @param key - Object key.
     * @returns Opened byte source result.
     */
    async invokeSourceOpen(ctx: SourceContext, key: string): Promise<ReadResult> {
      const src = await this.#plugin().source(ctx ?? { config: EMPTY_CONFIG });
      try {
        return await src.open(key);
      } finally {
        await disposeRpc(src);
      }
    }

    /**
     * Create worker, handle, dispose before returning.
     *
     * @param ctx - Worker factory context.
     * @param invocation - Durable command envelope.
     * @param grantToken - Per-invocation grant token.
     * @param databases - Per-binding grant tokens for named plugin databases.
     * @returns Job outcome from the handler.
     */
    async invokeHandle(
      ctx: WorkerContext,
      invocation: JobInvocation,
      grantToken: string,
      databases?: Record<string, string>,
    ): Promise<JobOutcome> {
      const handler = await this.#plugin().worker(ctx ?? { jobId: "", config: EMPTY_CONFIG });
      const controller = new AbortController();
      try {
        const context = grantedContext(this.env, grantToken, controller, databases);
        return await handler.handle(invocation, context);
      } finally {
        controller.abort();
        await disposeRpc(handler);
      }
    }
  };
}

function unsupported(method: string): Error {
  return PluginError.fromWire("unsupported", `${method} not implemented`);
}
