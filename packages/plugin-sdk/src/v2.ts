/**
 * TypeScript class ABI for `apiVersion` 2 (object-capability Workers RPC).
 *
 * Authors subclass {@link BookclerkPlugin} and return {@link Destination} /
 * {@link Source} / {@link JobHandler} RpcTargets. Byte payloads move as
 * `ReadableStream` — never as base64 scalars, `handleId`, or `writeChunk`.
 */

import "./cloudflare-workers.d.ts";
import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";

/** Product ABI version (`plugin.toml` `api_version` and `describe().apiVersion`). */
export const PRODUCT_API_VERSION = 2 as const;

/** Maximum decoded size of an ordinary RPC scalar value (not a stream window). */
export const MAX_SCALAR_BYTES = 262_144;

/** Maximum bytes returned by one stream pull window. */
export const MAX_STREAM_WINDOW_BYTES = 1_048_576;

/** Maximum objects in one `Destination.list` page. */
export const MAX_LIST_PAGE = 256;

/** Guest honors scalar / stream-window / list-page caps. */
export const FEATURE_SCALAR_LIMITS = "rpc.scalarLimits";

/** Media moves through transferred ReadableStream / ByteSource streams. */
export const FEATURE_STREAMS = "rpc.streams";

/** Guest implements server-side {@link Destination.copy}. */
export const FEATURE_STORAGE_COPY = "storage.copy";

/** Negotiated numeric limits advertised at {@link PluginDescribe}. */
export interface ScalarLimits {
  maxScalarBytes: number;
  maxStreamWindowBytes: number;
  maxListPage: number;
}

/** Maximum checkpoint payload size (bytes). */
export const MAX_CHECKPOINT_BYTES = 65_536;

/** Maximum domain-event scalar payload size (bytes). */
export const MAX_EVENT_PAYLOAD_BYTES = 65_536;

/** Guest identity returned by `BookclerkPlugin.describe`. */
export interface PluginDescribe {
  apiVersion: typeof PRODUCT_API_VERSION | 2;
  id: string;
  kind: string;
  displayName?: string;
  rpcFeatures: string[];
  scalarLimits: ScalarLimits;
  abiMajor?: number;
  abiMinor?: number;
  supportedRoles?: string[];
  metadataJson?: string;
}

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

/** Injected destination knobs. Opaque JSON only — no OS paths. */
export interface DestinationContext {
  json?: string;
}

/** Injected source knobs. Opaque JSON only — no OS paths. */
export interface SourceContext {
  json?: string;
}

/** Job worker instantiation knobs. Opaque JSON only — no OS paths. */
export interface WorkerContext {
  jobId?: string;
  json?: string;
}

/** Current envelope schema version for {@link JobInvocation}. */
export const ENVELOPE_VERSION = 1 as const;

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
  envelopeVersion: number;
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
  /** Producer plugin id; empty/omitted when unknown (`abiMinor` ≥ 6). */
  source?: string;
  correlationId?: string;
  causationId?: string;
  deduplicationKey?: string;
  deliveryAttempt: number;
  payload?: Uint8Array;
  /** Checkpoint JSON from a prior `suspended` result (`abiMinor` ≥ 5). */
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
      /** Event type that can wake this sleep; empty = timestamp-only (`abiMinor` ≥ 6). */
      wakeOnEventType?: string;
      /** Host-owned payload object filter JSON; empty = type only (`abiMinor` ≥ 6). */
      wakeOnFilterJson?: string;
    };

/** Author-visible granted bindings. Adapter-private tokens are not present. */
export interface GrantedBindings {
  HTTP?: BookclerkPluginEnv["HTTP"];
  STORAGE?: unknown;
  SECRETS?: unknown;
  OAUTH?: unknown;
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
  contentSource?(ctx: DestinationContext): ContentSource | Promise<ContentSource>;
  integration?(ctx: DestinationContext): Integration | Promise<Integration>;
  database?(ctx: DestinationContext): Database | Promise<Database>;
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

/** Storefront content source (not byte {@link Source}). */
export class ContentSource extends RpcTarget {
  /**
   * Completes an interactive or password login.
   *
   * @param _paramsJson - Login parameters as JSON.
   * @returns JSON result for the host/CLI.
   */
  login(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("login"));
  }
  /**
   * Scans the connected account library into JSON rows.
   *
   * @param _paramsJson - Scan parameters as JSON.
   * @returns JSON scan result.
   */
  scan(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("scan"));
  }
  /**
   * Fetches one title by storefront identifier.
   *
   * @param _paramsJson - Title identifiers as JSON.
   * @returns JSON title payload.
   */
  fetchTitle(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("fetchTitle"));
  }
  /**
   * Lists connected accounts for this plugin.
   *
   * @returns JSON account list.
   */
  listAccounts(): Promise<string> {
    return Promise.reject(unsupported("listAccounts"));
  }
  /**
   * Starts a multi-step login and returns a continuation payload.
   *
   * @param _paramsJson - Login-start parameters as JSON.
   * @returns JSON continuation for {@link ContentSource.loginComplete}.
   */
  loginStart(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("loginStart"));
  }
  /**
   * Finishes a login started by {@link ContentSource.loginStart}.
   *
   * @param _paramsJson - Continuation plus user input as JSON.
   * @returns JSON login result.
   */
  loginComplete(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("loginComplete"));
  }
  /**
   * Searches the storefront catalog.
   *
   * @param _paramsJson - Query parameters as JSON.
   * @returns JSON search hits.
   */
  searchCatalog(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("searchCatalog"));
  }
  /**
   * Expands a catalog hit into purchase/download candidates.
   *
   * @param _paramsJson - Candidate parameters as JSON.
   * @returns JSON candidate list.
   */
  expandCandidates(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("expandCandidates"));
  }
  /**
   * Returns a purchase hint for a catalog title.
   *
   * @param _paramsJson - Title identifiers as JSON.
   * @returns JSON hint payload.
   */
  purchaseHint(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Lists current storefront deals.
   *
   * @param _paramsJson - Optional filter JSON.
   * @returns JSON deal list.
   */
  listDeals(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("listDeals"));
  }
  /**
   * Loads catalog detail for one title.
   *
   * @param _paramsJson - Title identifiers as JSON.
   * @returns JSON detail payload.
   */
  catalogDetail(_paramsJson?: string): Promise<string> {
    return Promise.reject(unsupported("catalogDetail"));
  }
  /**
   * Runs plugin diagnostics and returns probe lines.
   *
   * @returns JSON array of diagnostic strings.
   */
  diagnose(): Promise<string> {
    return Promise.resolve("[]");
  }
  /**
   * Reports whether the storefront session is usable.
   *
   * @returns Health flag plus optional detail.
   */
  health(): Promise<{ ok: boolean; detail?: string }> {
    return Promise.resolve({ ok: true });
  }
}

/** Integration role. {@link Integration.onEvent} is not a generic job container. */
export class Integration extends RpcTarget {
  /**
   * Reports whether the integration session is usable.
   *
   * @returns Health flag plus optional detail.
   */
  health(): Promise<{ ok: boolean; detail?: string }> {
    return Promise.resolve({ ok: true });
  }
  /**
   * Runs integration diagnostics.
   *
   * @returns Probe lines as JSON or a `{ lines }` object.
   */
  diagnose(): Promise<string | { lines: string[] }> {
    return Promise.resolve({ lines: [] });
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
}

/** Database factory. Sessions cannot survive suspension. */
export class Database extends RpcTarget {
  openSession(): Promise<DatabaseSession> {
    return Promise.reject(unsupported("openSession"));
  }
}

/** Invocation-scoped database session. */
export class DatabaseSession extends RpcTarget {
  execute(_sql: string, _valuesJson?: string): Promise<{ lastInsertId: number; rowsAffected: number }> {
    return Promise.reject(unsupported("execute"));
  }
  query(
    _sql: string,
    _valuesJson?: string,
    _cursor?: string,
    _limit?: number,
  ): Promise<{ rowsJson: string; nextCursor?: string }> {
    return Promise.reject(unsupported("query"));
  }
  begin(): Promise<DbTransaction> {
    return Promise.reject(unsupported("begin"));
  }
  close(): Promise<void> {
    return Promise.resolve();
  }
}

/** Invocation-scoped transaction. */
export class DbTransaction extends RpcTarget {
  execute(_sql: string, _valuesJson?: string): Promise<{ lastInsertId: number; rowsAffected: number }> {
    return Promise.reject(unsupported("execute"));
  }
  query(
    _sql: string,
    _valuesJson?: string,
    _cursor?: string,
    _limit?: number,
  ): Promise<{ rowsJson: string; nextCursor?: string }> {
    return Promise.reject(unsupported("query"));
  }
  commit(): Promise<void> {
    return Promise.reject(unsupported("commit"));
  }
  rollback(): Promise<void> {
    return Promise.reject(unsupported("rollback"));
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

function v2GrantedContext(
  env: AdapterEnv,
  grantToken: string,
  controller: AbortController,
): JobContext {
  const granted = env.GRANTED;
  if (!granted || typeof grantToken !== "string" || !grantToken) {
    throw PluginError.fromWire("internal", "granted reverse channel missing");
  }
  const auth = { Authorization: `Bearer ${grantToken}` };
  return {
    input: new GrantedSource(granted, auth, controller.signal),
    output: new GrantedDestination(granted, auth, controller.signal),
    progress: new GrantedProgress(granted, auth, controller.signal),
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
   * @param _context - Opaque JSON knobs (no OS paths).
   */
  destination(_context: DestinationContext): Destination | Promise<Destination> {
    throw unsupported("destination");
  }

  /**
   * Returns a source capability for this invocation.
   *
   * @param _context - Opaque JSON knobs (no OS paths).
   */
  source(_context: SourceContext): Source | Promise<Source> {
    throw unsupported("source");
  }

  /**
   * Returns a job handler for this invocation.
   *
   * @param _context - Job id plus opaque JSON knobs (no OS paths).
   */
  worker(_context: WorkerContext): JobHandler | Promise<JobHandler> {
    throw unsupported("worker");
  }

  /**
   * Returns a storefront content-source capability.
   *
   * @param _ctx - Frozen invocation context.
   */
  contentSource(_ctx: BookclerkContext): ContentSource | Promise<ContentSource> {
    throw unsupported("contentSource");
  }

  /**
   * Returns an integration capability.
   *
   * @param _ctx - Frozen invocation context.
   */
  integration(_ctx: BookclerkContext): Integration | Promise<Integration> {
    throw unsupported("integration");
  }

  /**
   * Returns a database factory.
   *
   * @param _ctx - Frozen invocation context.
   */
  database(_ctx: BookclerkContext): Database | Promise<Database> {
    throw unsupported("database");
  }

  /**
   * Guest CLI schema JSON (`CliSchema`).
   *
   * @returns Schema JSON object string or empty object.
   */
  async cliDescribe(): Promise<string> {
    return "{}";
  }

  /**
   * Invokes a guest CLI command.
   *
   * @param _paramsJson - `CliInvokeParams` JSON.
   * @returns `CliInvokeResult` JSON.
   */
  async cliInvoke(_paramsJson: string): Promise<string> {
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
 * behavior with `__v2*` names.
 *
 * @returns Wrapper entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapV2PluginFromBinding() {
  return createInvocationAdapter();
}

/**
 * Native-behind-workerd generated adapter. Forwards through `ctx.native`.
 * `PLUGIN_BACKEND` stays private workerd config.
 *
 * @returns Wrapper entrypoint class bound to {@link AdapterEnv}.
 */
export function wrapV2PluginFromNative() {
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

class HttpNativeDest extends Destination {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };
  #ctx: DestinationContext;

  constructor(
    fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ctx: DestinationContext,
  ) {
    super();
    this.#fetcher = fetcher;
    this.#ctx = ctx ?? {};
  }

  async head(key: string) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/head", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ key, json: this.#ctx.json }),
    });
    const value = await readNativeScalar<{ found?: boolean; meta?: ObjectMetadata }>(resp);
    return value.found ? (value.meta ?? null) : null;
  }

  async list(options: ListOptions) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/list", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ options, json: this.#ctx.json }),
    });
    return assertListPage(await readNativeScalar<ListPage>(resp));
  }

  async get(key: string, options?: ReadOptions) {
    let path = `/v2/destination/get?key=${encodeURIComponent(key)}`;
    if (options?.range) {
      path += `&offset=${options.range.offset}`;
      if (options.range.length != null) path += `&length=${options.range.length}`;
    }
    const resp = await this.#fetcher.fetch(`http://backend${path}`, {
      headers: { "x-bookclerk-context": JSON.stringify(this.#ctx) },
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
      "x-bookclerk-context": JSON.stringify(this.#ctx),
    };
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) headers["content-length"] = String(options.contentLength);
    if (options?.commitToken) headers["x-bookclerk-commit-token"] = options.commitToken;
    if (options?.stageOnly) headers["x-bookclerk-stage-only"] = "1";
    const resp = await this.#fetcher.fetch(
      `http://backend/v2/destination/put?key=${encodeURIComponent(key)}`,
      { method: "PUT", headers, body },
    );
    return readNativeScalar<PutResult>(resp);
  }

  async copy(from: string, to: string) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/copy", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ from, to, json: this.#ctx.json }),
    });
    return readNativeScalar<CopyResult>(resp);
  }

  async delete(key: string) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/delete", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ key, json: this.#ctx.json }),
    });
    await readNativeScalar<unknown>(resp);
  }

  async commit(key: string, commitToken: string) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/commit", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ key, commitToken, json: this.#ctx.json }),
    });
    return readNativeScalar<PutResult>(resp);
  }

  async abortStage(key: string, commitToken: string) {
    const resp = await this.#fetcher.fetch("http://backend/v2/destination/abortStage", {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify({ key, commitToken, json: this.#ctx.json }),
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
    this.#ctx = ctx ?? {};
  }

  async open(key: string) {
    const resp = await this.#fetcher.fetch(
      `http://backend/v2/source/open?key=${encodeURIComponent(key)}`,
      { headers: { "x-bookclerk-context": JSON.stringify(this.#ctx) } },
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
  #ctx: { json?: string };

  constructor(
    fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch },
    ctx: { json?: string },
  ) {
    super();
    this.#fetcher = fetcher;
    this.#ctx = ctx ?? {};
  }

  async #json<T>(path: string, body: unknown): Promise<T> {
    const resp = await this.#fetcher.fetch(`http://backend${path}`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-bookclerk-context": JSON.stringify(this.#ctx),
      },
      body: JSON.stringify(body),
    });
    return readNativeScalar<T>(resp);
  }

  health() {
    return this.#json<{ ok: boolean; detail?: string }>("/v2/integration/health", {
      json: this.#ctx.json,
    });
  }

  diagnose() {
    return this.#json<string | { lines: string[] }>("/v2/integration/diagnose", {
      json: this.#ctx.json,
    });
  }

  onEvent(event: DomainEvent) {
    return this.#json<EventResult>("/v2/integration/onEvent", {
      json: this.#ctx.json,
      event,
    });
  }

  async start() {
    await this.#json<unknown>("/v2/integration/start", { json: this.#ctx.json });
  }

  async stop() {
    await this.#json<unknown>("/v2/integration/stop", { json: this.#ctx.json });
  }
}

class HttpNativeRoot {
  #fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch };

  constructor(fetcher: NonNullable<AdapterEnv["PLUGIN_BACKEND"]> & { fetch: typeof fetch }) {
    this.#fetcher = fetcher;
  }

  async describe() {
    const resp = await this.#fetcher.fetch("http://backend/v2/describe", {
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

  integration(ctx: { json?: string }) {
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

    contentSource(ctx: BookclerkContext): ContentSource | Promise<ContentSource> {
      return this.#plugin().contentSource(ctx);
    }

    integration(ctx: BookclerkContext): Integration | Promise<Integration> {
      return this.#plugin().integration(ctx);
    }

    database(ctx: BookclerkContext): Database | Promise<Database> {
      return this.#plugin().database(ctx);
    }

    async cliDescribe(): Promise<string> {
      return this.#plugin().cliDescribe();
    }

    async cliInvoke(paramsJson: string): Promise<string> {
      return this.#plugin().cliInvoke(paramsJson);
    }

    async oidcClients(): Promise<OidcClientTemplate[]> {
      const fn = this.#plugin().oidcClients;
      if (typeof fn !== "function") {
        return [];
      }
      const clients = await fn.call(this.#plugin());
      return Array.isArray(clients) ? clients : [];
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
      const dest = await this.#plugin().destination(ctx ?? {});
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
      const src = await this.#plugin().source(ctx ?? {});
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
     * @returns Job outcome from the handler.
     */
    async invokeHandle(
      ctx: WorkerContext,
      invocation: JobInvocation,
      grantToken: string,
    ): Promise<JobOutcome> {
      const handler = await this.#plugin().worker(ctx ?? {});
      const controller = new AbortController();
      try {
        const context = v2GrantedContext(this.env, grantToken, controller);
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
