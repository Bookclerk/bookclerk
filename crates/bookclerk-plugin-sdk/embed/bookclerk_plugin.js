/**
 * Workerd runtime for `@bookclerk/plugin-sdk` / `@bookclerk/plugin-sdk/workerd`.
 *
 * Authors import the package — never a relative embed path:
 *
 *   import { BookclerkEntrypoint, StorefrontEntrypoint } from "@bookclerk/plugin-sdk/workerd";
 *
 * `bookclerk-workerd` injects this module into the isolate under those names.
 * Native guests use Rust `serve` / `PluginWorker` instead.
 *
 * Author model (Workers idioms):
 *
 * - The default export extends `BookclerkEntrypoint`; triggers are handler
 *   methods on it: `event(batch)` for `[[events.consumers]]` and
 *   `job(job)` for `[triggers] jobs`.
 * - Capabilities the host calls are named exported classes
 *   (`Storefront`, `Storage`, `RemoteLibrary`, `DatabaseAdapter`, `Cli`,
 *   `Oidc`) extending the matching `*Entrypoint` base with typed methods.
 * - Everything the host provides is a binding on `env` (`CONFIG`, `SECRETS`,
 *   `EVENTS`, …); per-invocation facilities ride on the handler's controller
 *   object (`EventMessage`, `JobController`), never on `env`.
 *
 * The trusted adapter isolate (`wrapPluginFromBinding` /
 * `wrapPluginFromNative`) maps the host wire onto those idioms through the
 * `bookclerk*` dispatch methods the base classes define. Authors never see
 * `PLUGIN`, `PLUGIN_BACKEND`, `GRANTED`, or `BRIDGE_TOKEN`.
 */

import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";

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

/** Thrown when a wire union carries `err`. Unknown codes are kept on `wireCode`. */
export class PluginError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
  }

  static fromWire(code, message) {
    return new PluginError(code, message);
  }
}

function utf8Bytes(value) {
  return new TextEncoder().encode(String(value ?? "")).byteLength;
}

function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/**
 * Typed ABI value → native-broker JSON: `Uint8Array` fields (Cap'n `Data`)
 * travel as base64 text. Bookclerk-workerd decodes with the same convention.
 */
function toBridgeJson(value) {
  if (value instanceof Uint8Array) return bytesToBase64(value);
  if (value instanceof ArrayBuffer) return bytesToBase64(new Uint8Array(value));
  if (Array.isArray(value)) return value.map(toBridgeJson);
  if (value === null || typeof value !== "object") return value;
  const out = {};
  for (const [key, inner] of Object.entries(value)) {
    if (inner === undefined) continue;
    out[key] = toBridgeJson(inner);
  }
  return out;
}

function migrationOpSql(op) {
  if (op && typeof op === "object") {
    if (typeof op.schema === "string") return op.schema;
    if (typeof op.data === "string") return op.data;
  }
  return "";
}

function requirePluginMigrationRegistration(migrations) {
  const list = Array.isArray(migrations) ? migrations : [];
  if (list.length > MAX_LIST_PAGE) {
    throw PluginError.fromWire(
      "payload_too_large",
      `plugin migration count ${list.length} exceeds maxListPage (${MAX_LIST_PAGE})`,
    );
  }
  let total = 0;
  let totalOps = 0;
  for (const migration of list) {
    const ops = Array.isArray(migration?.operations) ? migration.operations : [];
    if (ops.length > MAX_PLUGIN_MIGRATION_OPS) {
      throw PluginError.fromWire(
        "payload_too_large",
        `plugin migration \`${migration?.id}\` has ${ops.length} operations; exceeds maxPluginMigrationOps (${MAX_PLUGIN_MIGRATION_OPS})`,
      );
    }
    totalOps += ops.length;
    if (totalOps > MAX_PLUGIN_MIGRATION_TOTAL_OPS) {
      throw PluginError.fromWire(
        "payload_too_large",
        `plugin migration registration has ${totalOps} operations; exceeds maxPluginMigrationTotalOps (${MAX_PLUGIN_MIGRATION_TOTAL_OPS})`,
      );
    }
    total += utf8Bytes(migration?.id);
    if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
      throw PluginError.fromWire(
        "payload_too_large",
        `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
      );
    }
    for (const op of ops) {
      const n = utf8Bytes(migrationOpSql(op));
      if (n > MAX_SCALAR_BYTES) {
        throw PluginError.fromWire(
          "payload_too_large",
          `plugin migration \`${migration?.id}\` SQL is ${n} bytes; exceeds maxScalarBytes (${MAX_SCALAR_BYTES})`,
        );
      }
      total += n;
      if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
        throw PluginError.fromWire(
          "payload_too_large",
          `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
        );
      }
    }
  }
  return list;
}

function unsupportedMethod(method) {
  return PluginError.fromWire("unsupported", `${method} not implemented`);
}

/** Product ABI version 3 (`describe().apiVersion`). */
export const PRODUCT_API_VERSION = 3;
export const MAX_SCALAR_BYTES = 262144;
export const MAX_STREAM_WINDOW_BYTES = 1048576;
export const MAX_LIST_PAGE = 256;
export const MAX_PLUGIN_MIGRATION_OPS = 256;
export const MAX_PLUGIN_MIGRATION_TOTAL_OPS = 2048;
export const MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144;
export const FEATURE_SCALAR_LIMITS = "rpc.scalarLimits";
export const FEATURE_STREAMS = "rpc.streams";
export const FEATURE_STORAGE_COPY = "storage.copy";
export const MAX_CHECKPOINT_BYTES = 65536;
export const MAX_EVENT_PAYLOAD_BYTES = 65536;

// ---------------------------------------------------------------------------
// Capability shapes (host-served objects a handler receives on its controller
// or as a binding). Byte payloads move as `ReadableStream`, never as scalars.
// ---------------------------------------------------------------------------

/** Object store: `job.output` and the `WORK_FS` binding. Abort is stream cancel. */
export class Destination extends RpcTarget {
  async head(_key) {
    throw unsupportedMethod("head");
  }
  async list(_options) {
    throw unsupportedMethod("list");
  }
  async get(_key, _options) {
    throw unsupportedMethod("get");
  }
  async put(_key, _body, _options) {
    throw unsupportedMethod("put");
  }
  async copy(_from, _to) {
    throw unsupportedMethod("copy");
  }
  async delete(_key) {
    throw unsupportedMethod("delete");
  }
  async commit(_key, _commitToken) {
    throw unsupportedMethod("commit");
  }
  async abortStage(_key, _commitToken) {
    throw unsupportedMethod("abortStage");
  }
}

/** Byte source: `job.input`. */
export class Source extends RpcTarget {
  async open(_key) {
    throw unsupportedMethod("open");
  }
}

/** Progress reports (never media). */
export class ProgressSink extends RpcTarget {
  async report(_percent, _message) {
    throw unsupportedMethod("report");
  }
}

/** Host ↔ database adapter session returned by `DatabaseAdapterEntrypoint.openSession`. */
export class AdapterDatabaseSession extends RpcTarget {
  async capabilities() {
    throw unsupportedMethod("capabilities");
  }
  async execute(_request) {
    throw unsupportedMethod("execute");
  }
  async close() {}
}

// ---------------------------------------------------------------------------
// Bindings on `env`
// ---------------------------------------------------------------------------

/**
 * Decode an `ExtensibleConfig` (`{ schemaVersion, mediaType, payload }`) into
 * the plain object authors read from `env.CONFIG` / `env.SECRETS`. Non-JSON
 * media types are surfaced verbatim so nothing is lost.
 */
function decodeExtensibleConfig(cfg) {
  if (cfg == null) return {};
  if (typeof cfg !== "object") return cfg;
  const payload = cfg.payload;
  let text = "";
  if (payload instanceof Uint8Array) {
    text = new TextDecoder().decode(payload);
  } else if (payload instanceof ArrayBuffer) {
    text = new TextDecoder().decode(new Uint8Array(payload));
  } else if (typeof payload === "string") {
    text = payload;
  } else if (payload && typeof payload === "object" && !("schemaVersion" in cfg)) {
    return payload;
  }
  const mediaType = String(cfg.mediaType ?? "");
  if (!text.trim()) return {};
  if (mediaType === "" || /json/i.test(mediaType)) {
    try {
      const parsed = JSON.parse(text);
      return parsed === null || typeof parsed !== "object" ? {} : parsed;
    } catch {
      return {};
    }
  }
  return { mediaType, text };
}

/**
 * Wrap a JSON-serializable value as an `application/json` `ExtensibleConfig`
 * (schema version 1) — the shape of `CliInvokeResult.payload` and every other
 * extensible payload on the wire.
 */
export function jsonPayload(value) {
  return {
    schemaVersion: 1,
    mediaType: "application/json",
    payload: new TextEncoder().encode(JSON.stringify(value ?? null)),
  };
}

/**
 * Turn `CliInvokeParams.args` (`[{ name, value }]`) into a plain
 * `{ name: value }` object for CLI handlers.
 */
export function cliArgs(params) {
  const out = {};
  for (const arg of Array.isArray(params?.args) ? params.args : []) {
    if (arg && typeof arg.name === "string") out[arg.name] = String(arg.value ?? "");
  }
  return out;
}

/**
 * Build the per-invocation `env` view: static isolate bindings plus the
 * granted `Bindings` the host delivered on `PluginWorker.open`.
 */
function invocationEnv(rawEnv, context) {
  const merged = { ...(rawEnv ?? {}) };
  if (context && typeof context === "object") {
    if (context.config !== undefined) merged.CONFIG = decodeExtensibleConfig(context.config);
    if (context.secrets !== undefined) merged.SECRETS = decodeExtensibleConfig(context.secrets);
    if (context.storage) merged.WORK_FS = context.storage;
    if (context.events) merged.EVENTS = context.events;
    if (Array.isArray(context.databases)) {
      for (const entry of context.databases) {
        if (entry && typeof entry.name === "string" && entry.database) {
          merged[entry.name] = entry.database;
        }
      }
    }
  }
  return Object.freeze(merged);
}

function applyInvocationEnv(instance, context) {
  const merged = invocationEnv(instance.env, context);
  try {
    instance.env = merged;
  } catch {
    Object.defineProperty(instance, "env", { value: merged, configurable: true });
  }
  return merged;
}

/** Invocation envelope (`Invocation` struct) with zero values normalized. */
function invocationOf(context) {
  const inv = context && typeof context === "object" ? context.invocation ?? {} : {};
  return Object.freeze({
    id: String(inv.id ?? ""),
    accountId: String(inv.accountId ?? ""),
    deadlineUnixMs: Number(inv.deadlineUnixMs ?? 0) || 0,
    correlationId: String(inv.correlationId ?? ""),
    causationId: String(inv.causationId ?? ""),
  });
}

// ---------------------------------------------------------------------------
// Event trigger: `event(batch)` shaped like Workers `queue(batch)`
// ---------------------------------------------------------------------------

function unixMs(value) {
  if (value instanceof Date) return value.getTime();
  const n = Number(value ?? 0);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}

function checkpointText(checkpoint) {
  if (checkpoint === undefined || checkpoint === null) return "";
  const text = typeof checkpoint === "string" ? checkpoint : JSON.stringify(checkpoint);
  if (utf8Bytes(text) > MAX_CHECKPOINT_BYTES) {
    throw PluginError.fromWire(
      "payload_too_large",
      `checkpoint is ${utf8Bytes(text)} bytes; exceeds maxCheckpointBytes (${MAX_CHECKPOINT_BYTES})`,
    );
  }
  return text;
}

/**
 * One delivered domain event. Exactly one outcome is recorded per message;
 * the first of `ack` / `retry` / `reject` / `deadLetter` / `suspend` wins and
 * later calls are ignored (as with Workers queue messages). Messages left
 * untouched are acked when `event()` resolves and retried when it throws.
 */
export class EventMessage {
  #result = null;

  constructor(event) {
    const e = event && typeof event === "object" ? event : {};
    this.id = String(e.eventId ?? "");
    this.type = String(e.eventType ?? "");
    this.schemaVersion = Number(e.schemaVersion ?? 0) || 0;
    this.timestamp = new Date(Number(e.occurredAtUnixMs ?? 0) || 0);
    this.attempts = Number(e.deliveryAttempt ?? 1) || 1;
    this.accountId = String(e.accountId ?? "");
    this.source = String(e.source ?? "");
    this.correlationId = String(e.correlationId ?? "");
    this.causationId = String(e.causationId ?? "");
    this.deduplicationKey = String(e.deduplicationKey ?? "");
    this.body =
      e.payload instanceof Uint8Array
        ? e.payload
        : e.payload instanceof ArrayBuffer
          ? new Uint8Array(e.payload)
          : new Uint8Array();
    this.invocationSequence = Number(e.invocationSequence ?? 0) || 0;
    this.resumePending = Boolean(e.resumePending);
    const checkpointJson = String(e.checkpointJson ?? "");
    this.checkpoint = checkpointJson
      ? Object.freeze({
          json: checkpointJson,
          schemaVersion: Number(e.checkpointSchemaVersion ?? 0) || 0,
        })
      : null;
    this.raw = e;
  }

  /** Decode `body` as JSON (`{}` for an empty body). */
  json() {
    if (this.body.byteLength === 0) return {};
    return JSON.parse(new TextDecoder().decode(this.body));
  }

  /** Recorded wire `EventResult`, or `null` while undecided. */
  get result() {
    return this.#result;
  }

  #record(result) {
    if (this.#result === null) this.#result = Object.freeze(result);
  }

  /** Mark handled; the host marks the event delivered. */
  ack() {
    this.#record({ kind: "ack" });
  }

  /**
   * Redeliver later: at `retryAt` (Date or Unix ms), after `delaySeconds`,
   * or — when both are omitted — whenever the host's backoff chooses.
   */
  retry(options) {
    const explicit = unixMs(options?.retryAt);
    const delay = Number(options?.delaySeconds ?? 0) || 0;
    this.#record({
      kind: "retry",
      retryAtUnixMs: explicit || (delay > 0 ? Date.now() + Math.floor(delay * 1000) : 0),
      reason: String(options?.reason ?? ""),
    });
  }

  /** Stop delivering; the host records `reason`. */
  reject(reason) {
    this.#record({ kind: "reject", reason: String(reason ?? "") });
  }

  /** Park for operator review. */
  deadLetter(reason) {
    this.#record({ kind: "deadLetter", reason: String(reason ?? "") });
  }

  /**
   * Release with a bounded checkpoint; the host redelivers at `wakeAt`
   * (Date or Unix ms) or when a `wakeOnEventType` event arrives.
   */
  suspend(options) {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpointJson: checkpointText(opts.checkpoint),
      checkpointSchemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
      wakeAtUnixMs: unixMs(opts.wakeAt),
      wakeOnEventType: String(opts.wakeOnEventType ?? ""),
      wakeOnFilterJson:
        opts.wakeOnFilter === undefined || opts.wakeOnFilter === null
          ? ""
          : typeof opts.wakeOnFilter === "string"
            ? opts.wakeOnFilter
            : JSON.stringify(opts.wakeOnFilter),
    });
  }
}

/** One `event(batch)` delivery (`EventBatch` struct), Workers `MessageBatch`-shaped. */
export class EventBatch {
  constructor(events) {
    this.messages = Object.freeze(
      (Array.isArray(events) ? events : []).map((event) => new EventMessage(event)),
    );
  }

  /** Event type shared by the batch (`""` when mixed or empty). */
  get type() {
    const first = this.messages[0]?.type ?? "";
    return this.messages.every((m) => m.type === first) ? first : "";
  }

  ackAll() {
    for (const m of this.messages) m.ack();
  }

  retryAll(options) {
    for (const m of this.messages) m.retry(options);
  }
}

function errorMessage(err) {
  return err instanceof Error ? err.message : String(err);
}

/**
 * Translate recorded `EventMessage` outcomes into the wire `EventResult` list.
 * `failed` is the error thrown by the author's `event()`; undecided messages
 * then retry instead of ack.
 */
function eventBatchResults(batch, failed) {
  return batch.messages.map((m) => {
    if (m.result) return m.result;
    if (failed !== undefined) {
      return { kind: "retry", retryAtUnixMs: 0, reason: errorMessage(failed) };
    }
    return { kind: "ack" };
  });
}

// ---------------------------------------------------------------------------
// Job trigger: `job(controller)` shaped like Workers `scheduled(controller)`
// ---------------------------------------------------------------------------

/**
 * Everything one job invocation may touch: the durable envelope, `input` /
 * `output` streams, `progress()`, `signal`, and the terminal-outcome recorders
 * `suspend()` / `retryLater()`. Returning normally without a recorded outcome
 * completes the job; throwing rejects it (`unavailable` /
 * `deadline_exceeded` errors are retryable, `cancelled` is cancelled).
 */
export class JobController {
  #result = null;
  #progress;
  #abort;

  constructor(invocation, granted) {
    const inv = invocation && typeof invocation === "object" ? invocation : {};
    this.invocation = Object.freeze({ ...inv });
    this.id = String(inv.invocationId ?? "");
    this.type = String(inv.commandType ?? "");
    this.payloadJson = String(inv.payloadJson ?? "");
    this.payloadSchemaVersion = Number(inv.payloadSchemaVersion ?? 0) || 0;
    this.idempotencyKey = String(inv.idempotencyKey ?? "");
    this.attempt = Number(inv.attempt ?? 1) || 1;
    this.deadlineUnixMs = Number(inv.deadlineUnixMs ?? 0) || 0;
    this.correlationId = String(inv.correlationId ?? "");
    this.causationId = String(inv.causationId ?? "");
    this.invocationSequence = Number(inv.invocationSequence ?? 0) || 0;
    this.stepId = String(inv.stepId ?? "");
    const checkpointJson = String(inv.checkpointJson ?? "");
    this.checkpoint = checkpointJson
      ? Object.freeze({
          json: checkpointJson,
          schemaVersion: Number(inv.checkpointSchemaVersion ?? 0) || 0,
        })
      : null;
    this.input = granted?.input ?? null;
    this.output = granted?.output ?? null;
    this.#progress = granted?.progress ?? null;
    this.#abort = new AbortController();
    this.signal = this.#abort.signal;
    const cancel = granted?.cancel;
    if (cancel && typeof cancel.wait === "function") {
      Promise.resolve()
        .then(() => cancel.wait())
        .then(
          () => this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host")),
          () => {},
        );
    }
  }

  /** Decode `payloadJson` (`{}` when empty). */
  json() {
    return this.payloadJson ? JSON.parse(this.payloadJson) : {};
  }

  /** Report `percent` in `0..=100` with an operator-facing `message`. */
  async progress(percent, message) {
    if (!this.#progress) return;
    await this.#progress.report(Number(percent) || 0, String(message ?? ""));
  }

  /** Recorded terminal outcome, or `null` while the job is still running. */
  get result() {
    return this.#result;
  }

  #record(result) {
    if (this.#result === null) this.#result = Object.freeze(result);
  }

  /** Release with a bounded checkpoint; the host resumes at `wakeAt`. */
  suspend(options) {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpoint: {
        schemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
        json: checkpointText(opts.checkpoint),
      },
      wakeAtUnixMs: unixMs(opts.wakeAt),
    });
  }

  /** Give up this attempt and let the host retry at `retryAt` (or its default). */
  retryLater(options) {
    const opts = options ?? {};
    this.#record({
      kind: "retryable",
      message: String(opts.reason ?? ""),
      retryAfterUnixMs: unixMs(opts.retryAt),
    });
  }

  /** Host-side cancellation observed (adapter-internal). */
  cancel() {
    this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host"));
  }
}

function jobOutcomeFor(job, returned, failed) {
  if (job.result) return job.result;
  if (failed !== undefined) {
    const code = failed && typeof failed === "object" ? failed.wireCode ?? failed.code : undefined;
    if (job.signal.aborted || code === "cancelled") {
      return { kind: "cancelled", message: errorMessage(failed) };
    }
    if (code === "unavailable" || code === "deadline_exceeded") {
      return { kind: "retryable", message: errorMessage(failed), retryAfterUnixMs: 0 };
    }
    return { kind: "rejected", message: errorMessage(failed) };
  }
  const extra = returned && typeof returned === "object" ? returned : {};
  return {
    kind: "completed",
    message: String(extra.message ?? ""),
    bytesCopied: Number(extra.bytesCopied ?? 0) || 0,
  };
}

// ---------------------------------------------------------------------------
// Author base classes
// ---------------------------------------------------------------------------

/**
 * Default-export base. Optional trigger methods: `event(batch)` and
 * `job(job)`; optional `describe()` (identity beyond `plugin.toml`),
 * `databaseMigrations(binding)`, and `shutdown()`.
 *
 * The `bookclerk*` methods are the adapter-facing dispatch surface; authors
 * neither call nor override them.
 */
export class BookclerkEntrypoint extends WorkerEntrypoint {
  async fetch() {
    return new Response(null, { status: 404 });
  }

  async bookclerkDescribe() {
    if (typeof this.describe !== "function") return null;
    return await this.describe();
  }

  async bookclerkEvent(context, wireBatch) {
    if (typeof this.event !== "function") {
      throw unsupportedMethod("event");
    }
    applyInvocationEnv(this, context);
    const events = Array.isArray(wireBatch?.events) ? wireBatch.events : [];
    const batch = new EventBatch(events);
    batch.invocation = invocationOf(context);
    try {
      await this.event(batch);
      return eventBatchResults(batch, undefined);
    } catch (err) {
      return eventBatchResults(batch, err ?? new Error("event handler failed"));
    }
  }

  async bookclerkJob(context, invocation, granted) {
    if (typeof this.job !== "function") {
      throw unsupportedMethod("job");
    }
    applyInvocationEnv(this, context);
    const job = new JobController(invocation, granted);
    try {
      const returned = await this.job(job);
      return jobOutcomeFor(job, returned, undefined);
    } catch (err) {
      return jobOutcomeFor(job, undefined, err ?? new Error("job handler failed"));
    }
  }

  async bookclerkDatabaseMigrations(binding) {
    if (typeof this.databaseMigrations !== "function") return [];
    const migrations = await this.databaseMigrations(String(binding ?? ""));
    return requirePluginMigrationRegistration(Array.isArray(migrations) ? migrations : []);
  }

  async bookclerkShutdown() {
    if (typeof this.shutdown === "function") await this.shutdown();
  }
}

/**
 * Base for named entrypoints. Subclasses list their RPC surface on the static
 * `bookclerkMethods`; the adapter reaches them only through `bookclerkInvoke`,
 * which installs the granted bindings on `env` before dispatching.
 */
class NamedEntrypoint extends WorkerEntrypoint {
  static bookclerkMethods = [];

  async fetch() {
    return new Response(null, { status: 404 });
  }

  async bookclerkInvoke(context, method, ...args) {
    const allowed = this.constructor.bookclerkMethods ?? [];
    if (!allowed.includes(method) || typeof this[method] !== "function") {
      throw unsupportedMethod(method);
    }
    applyInvocationEnv(this, context);
    this.invocation = invocationOf(context);
    return await this[method](...args);
  }
}

const STOREFRONT_METHODS = Object.freeze([
  "login",
  "scan",
  "fetchTitle",
  "listAccounts",
  "loginStart",
  "loginComplete",
  "searchCatalog",
  "expandCandidates",
  "purchaseHint",
  "listDeals",
  "catalogDetail",
  "diagnose",
  "health",
]);

/**
 * `storefront` entrypoint (`ContentSource` interface). Every method takes and
 * returns the typed ABI structs (`LoginParams` → `LoginResult`, …) as plain
 * objects; see `generated.ts` for the shapes.
 */
export class StorefrontEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = STOREFRONT_METHODS;

  async login(_params) {
    throw unsupportedMethod("login");
  }
  async scan(_params) {
    throw unsupportedMethod("scan");
  }
  async fetchTitle(_params) {
    throw unsupportedMethod("fetchTitle");
  }
  async listAccounts() {
    throw unsupportedMethod("listAccounts");
  }
  async loginStart(_params) {
    throw unsupportedMethod("loginStart");
  }
  async loginComplete(_params) {
    throw unsupportedMethod("loginComplete");
  }
  async searchCatalog(_params) {
    throw unsupportedMethod("searchCatalog");
  }
  async expandCandidates(_params) {
    throw unsupportedMethod("expandCandidates");
  }
  async purchaseHint(_params) {
    throw unsupportedMethod("purchaseHint");
  }
  async listDeals(_params) {
    throw unsupportedMethod("listDeals");
  }
  async catalogDetail(_params) {
    throw unsupportedMethod("catalogDetail");
  }
  async health() {
    return { ok: true, detail: "" };
  }
  /** Operator-facing diagnostic lines (`string[]`). */
  async diagnose() {
    return [];
  }
}

const STORAGE_METHODS = Object.freeze([
  "head",
  "list",
  "get",
  "put",
  "copy",
  "delete",
  "commit",
  "abortStage",
]);

/** `storage` entrypoint (`Destination` interface): an object store the host writes to. */
export class StorageEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = STORAGE_METHODS;

  async head(_key) {
    throw unsupportedMethod("head");
  }
  async list(_options) {
    throw unsupportedMethod("list");
  }
  async get(_key, _options) {
    throw unsupportedMethod("get");
  }
  async put(_key, _body, _options) {
    throw unsupportedMethod("put");
  }
  async copy(_from, _to) {
    throw unsupportedMethod("copy");
  }
  async delete(_key) {
    throw unsupportedMethod("delete");
  }
  async commit(_key, _commitToken) {
    throw unsupportedMethod("commit");
  }
  async abortStage(_key, _commitToken) {
    throw unsupportedMethod("abortStage");
  }
}

const REMOTE_LIBRARY_METHODS = Object.freeze([
  "health",
  "diagnose",
  "start",
  "stop",
  "scanLibrary",
  "syncListening",
  "pollEvents",
]);

/** `remoteLibrary` entrypoint: lifecycle, rescan, listening sync, user polling. */
export class RemoteLibraryEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = REMOTE_LIBRARY_METHODS;

  async health() {
    return { ok: true, detail: "" };
  }
  /** Operator-facing diagnostic lines (`string[]`). */
  async diagnose() {
    return [];
  }
  async start() {}
  async stop() {}
  async scanLibrary(_params) {
    throw unsupportedMethod("scanLibrary");
  }
  /** `ListeningProgress[]`. */
  async syncListening() {
    throw unsupportedMethod("syncListening");
  }
  /** `ExternalUser[]` observed since the last poll. */
  async pollEvents() {
    throw unsupportedMethod("pollEvents");
  }
}

/** `databaseAdapter` entrypoint: opens typed SQL sessions for the host library. */
export class DatabaseAdapterEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["openSession"]);

  async openSession() {
    throw unsupportedMethod("openSession");
  }
}

/** `cli` entrypoint (`PluginCli`): `describe()` → `CliSchema`, `invoke(params)` → `CliInvokeResult`. */
export class CliEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["describe", "invoke"]);

  async describe() {
    return { commands: [] };
  }
  async invoke(_params) {
    throw unsupportedMethod("invoke");
  }
}

/** `oidc` entrypoint: relying-party client templates and credential verification. */
export class OidcEntrypoint extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["clients", "authenticateUser"]);

  async clients() {
    return [];
  }
  async authenticateUser(_params) {
    throw unsupportedMethod("authenticateUser");
  }
}

// ---------------------------------------------------------------------------
// Adapter isolate (trusted; owns GRANTED / BRIDGE_TOKEN / PLUGIN_BACKEND)
// ---------------------------------------------------------------------------

function exactLengthBody(body, expected) {
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

class GrantedSource extends RpcTarget {
  constructor(granted, auth, signal) {
    super();
    this.granted = granted;
    this.auth = auth;
    this.signal = signal;
  }
  async open(key) {
    const resp = await this.granted.fetch(
      `http://granted/open?key=${encodeURIComponent(key)}`,
      { headers: this.auth, signal: this.signal },
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
      body: resp.body,
    };
  }
}

class GrantedDestination extends RpcTarget {
  constructor(granted, auth, signal) {
    super();
    this.granted = granted;
    this.auth = auth;
    this.signal = signal;
  }
  async put(key, body, options) {
    const headers = { ...this.auth };
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) {
      headers["content-length"] = String(options.contentLength);
    }
    const resp = await this.granted.fetch(
      `http://granted/put?key=${encodeURIComponent(key)}`,
      { method: "PUT", headers, body, signal: this.signal },
    );
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return resp.json();
  }
}

class GrantedProgress extends RpcTarget {
  constructor(granted, auth, signal) {
    super();
    this.granted = granted;
    this.auth = auth;
    this.signal = signal;
  }
  async report(percent, message) {
    await this.granted.fetch(`http://granted/progress`, {
      method: "POST",
      headers: { ...this.auth, "content-type": "application/json" },
      body: JSON.stringify({ percent, message: message || "" }),
      signal: this.signal,
    });
  }
}

/** Encode a `PublishEvent.payload` (bytes, string, or JSON value) as bytes. */
function publishPayloadBytes(payload) {
  if (payload === undefined || payload === null) return new Uint8Array(0);
  if (payload instanceof Uint8Array) return payload;
  if (payload instanceof ArrayBuffer) return new Uint8Array(payload);
  if (typeof payload === "string") return new TextEncoder().encode(payload);
  return new TextEncoder().encode(JSON.stringify(payload));
}

/**
 * `EVENTS` binding handed to the author: the adapter exchanges the host's
 * events grant token on the granted channel, so the author isolate never
 * holds the bearer itself.
 */
class GrantedEvents extends RpcTarget {
  constructor(granted, auth) {
    super();
    this.granted = granted;
    this.auth = auth;
  }
  async publish(event) {
    if (!event || typeof event.eventType !== "string" || !event.eventType) {
      throw PluginError.fromWire("invalid_params", "publish requires eventType");
    }
    const payload = publishPayloadBytes(event.payload);
    if (payload.byteLength > MAX_EVENT_PAYLOAD_BYTES) {
      throw PluginError.fromWire(
        "payload_too_large",
        `event payload of ${payload.byteLength} bytes exceeds ${MAX_EVENT_PAYLOAD_BYTES}`,
      );
    }
    const wire = toBridgeJson({
      eventType: event.eventType,
      schemaVersion: Number(event.schemaVersion ?? 1) || 1,
      deduplicationKey: String(event.deduplicationKey ?? ""),
      payload,
      occurredAtUnixMs: Number(event.occurredAtUnixMs ?? 0) || 0,
      correlationId: String(event.correlationId ?? ""),
      causationId: String(event.causationId ?? ""),
    });
    const resp = await this.granted.fetch("http://granted/events/publish", {
      method: "POST",
      headers: { ...this.auth, "content-type": "application/json" },
      body: JSON.stringify(wire),
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events publish HTTP ${resp.status}`);
    }
    return { eventId: String(value.eventId ?? ""), duplicate: Boolean(value.duplicate) };
  }
}

/** Resolves `wait()` once the adapter observes host cancellation. */
class CancelWatch extends RpcTarget {
  constructor(signal) {
    super();
    this.promise = new Promise((resolve) => {
      if (signal.aborted) resolve();
      else signal.addEventListener("abort", () => resolve(), { once: true });
    });
  }
  wait() {
    return this.promise;
  }
}

function grantedJobCapabilities(env, grantToken, controller) {
  const granted = env.GRANTED;
  if (!granted || typeof grantToken !== "string" || !grantToken) {
    throw PluginError.fromWire("internal", "granted reverse channel missing");
  }
  const auth = { Authorization: `Bearer ${grantToken}` };
  return {
    input: new GrantedSource(granted, auth, controller.signal),
    output: new GrantedDestination(granted, auth, controller.signal),
    progress: new GrantedProgress(granted, auth, controller.signal),
    cancel: new CancelWatch(controller.signal),
  };
}

export function wrapPluginFromBinding() {
  return createInvocationAdapter();
}

export function wrapPluginFromNative() {
  return createInvocationAdapter();
}

async function readNativeJson(resp) {
  const value = await resp.json().catch(() => ({}));
  if (value && value.error) {
    throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
  }
  if (!resp.ok) {
    throw PluginError.fromWire("internal", `native broker HTTP ${resp.status}`);
  }
  return value;
}

function streamedRead(resp, key) {
  return {
    meta: {
      key: resp.headers.get("x-bookclerk-key") || key,
      size: Number(resp.headers.get("x-bookclerk-size") || "0"),
      contentType: resp.headers.get("x-bookclerk-content-type") || undefined,
      etag: resp.headers.get("x-bookclerk-etag") || undefined,
    },
    body: resp.body,
  };
}

/**
 * Native-behind-workerd backend (`PLUGIN_BACKEND` is the trusted broker's
 * HTTP surface). Entrypoint methods map onto the broker's role routes.
 */
class HttpNativeRoot {
  constructor(fetcher) {
    this.fetcher = fetcher;
  }

  #headers(ctx) {
    return {
      "content-type": "application/json",
      "x-bookclerk-context": JSON.stringify(toBridgeJson(ctx ?? {})),
    };
  }

  async #json(path, ctx, body) {
    const resp = await this.fetcher.fetch(`http://backend${path}`, {
      method: "POST",
      headers: this.#headers(ctx),
      body: JSON.stringify(toBridgeJson({ ...(body ?? {}), context: ctx ?? {} })),
    });
    return readNativeJson(resp);
  }

  async describe() {
    const resp = await this.fetcher.fetch("http://backend/describe", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    return readNativeJson(resp);
  }

  async storage(ctx, method, args) {
    const [a, b, c] = args;
    switch (method) {
      case "head": {
        const v = await this.#json("/destination/head", ctx, { key: String(a ?? "") });
        return v.found ? v.meta : null;
      }
      case "list":
        return this.#json("/destination/list", ctx, { options: a ?? {} });
      case "get": {
        let path = `/destination/get?key=${encodeURIComponent(String(a ?? ""))}`;
        if (b?.range) {
          path += `&offset=${b.range.offset}`;
          if (b.range.length != null) path += `&length=${b.range.length}`;
        }
        const resp = await this.fetcher.fetch(`http://backend${path}`, {
          headers: this.#headers(ctx),
        });
        if (!resp.ok) {
          throw PluginError.fromWire("internal", await resp.text());
        }
        return streamedRead(resp, String(a ?? ""));
      }
      case "put": {
        const headers = this.#headers(ctx);
        if (c?.contentType) headers["content-type"] = c.contentType;
        if (c?.contentLength != null) headers["content-length"] = String(c.contentLength);
        if (c?.commitToken) headers["x-bookclerk-commit-token"] = c.commitToken;
        if (c?.stageOnly) headers["x-bookclerk-stage-only"] = "1";
        const resp = await this.fetcher.fetch(
          `http://backend/destination/put?key=${encodeURIComponent(String(a ?? ""))}`,
          { method: "PUT", headers, body: b },
        );
        return readNativeJson(resp);
      }
      case "copy":
        return this.#json("/destination/copy", ctx, { from: a, to: b });
      case "delete":
        await this.#json("/destination/delete", ctx, { key: String(a ?? "") });
        return undefined;
      case "commit":
        return this.#json("/destination/commit", ctx, { key: a, commitToken: b });
      case "abortStage":
        await this.#json("/destination/abortStage", ctx, { key: a, commitToken: b });
        return undefined;
      default:
        throw unsupportedMethod(`storage.${method}`);
    }
  }

  async remoteLibrary(ctx, method, args) {
    if (!REMOTE_LIBRARY_METHODS.includes(method)) {
      throw unsupportedMethod(`remoteLibrary.${method}`);
    }
    const v = await this.#json(`/integration/${method}`, ctx, args[0] != null ? { params: args[0] } : {});
    if (method === "diagnose") {
      return Array.isArray(v) ? v : Array.isArray(v?.lines) ? v.lines : [];
    }
    if (method === "pollEvents") {
      return Array.isArray(v) ? v : Array.isArray(v?.users) ? v.users : [];
    }
    return v;
  }

  async event(ctx, event) {
    return this.#json("/integration/onEvent", ctx, { event });
  }

  async invoke(name, ctx, method, args) {
    switch (name) {
      case "storage":
        return this.storage(ctx, method, args);
      case "remoteLibrary":
        return this.remoteLibrary(ctx, method, args);
      case "oidc":
        if (method === "authenticateUser") {
          return this.#json("/integration/authenticateUser", ctx, { params: args[0] });
        }
        throw unsupportedMethod(`oidc.${method}`);
      default:
        throw unsupportedMethod(`${name}.${method} via native broker`);
    }
  }
}

function nativeRoot(env) {
  const backend = env.PLUGIN_BACKEND;
  if (!backend) {
    throw PluginError.fromWire("unavailable", "PLUGIN_BACKEND binding missing");
  }
  if (typeof backend.fetch === "function") {
    return new HttpNativeRoot(backend);
  }
  return backend;
}

/** Adapter binding name for each named entrypoint (`plugin.toml` `entrypoints`). */
const ENTRYPOINT_BINDINGS = Object.freeze({
  storefront: "PLUGIN_STOREFRONT",
  storage: "PLUGIN_STORAGE",
  databaseAdapter: "PLUGIN_DATABASE_ADAPTER",
  remoteLibrary: "PLUGIN_REMOTE_LIBRARY",
  cli: "PLUGIN_CLI",
  oidc: "PLUGIN_OIDC",
});

function parseJsonBinding(value) {
  if (value == null) return null;
  if (typeof value === "string") {
    try {
      return JSON.parse(value);
    } catch {
      return null;
    }
  }
  return typeof value === "object" ? value : null;
}

/** Bridge context without the adapter-private events grant token. */
function stripEventsToken(ctx) {
  const source = ctx && typeof ctx === "object" ? ctx : {};
  const { eventsToken: _eventsToken, ...rest } = source;
  return rest;
}

function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint {
    #native() {
      return this.env.PLUGIN_BACKEND && !this.env.PLUGIN ? nativeRoot(this.env) : null;
    }

    #author() {
      if (this.env.PLUGIN) return this.env.PLUGIN;
      throw PluginError.fromWire("unavailable", "PLUGIN binding missing");
    }

    #named(name) {
      const binding = ENTRYPOINT_BINDINGS[name];
      const stub = binding ? this.env[binding] : undefined;
      if (!stub) {
        throw PluginError.fromWire("unsupported", `${name} entrypoint not exported`);
      }
      return stub;
    }

    /**
     * Turn the bridge context into the author-facing one: the host's events
     * grant token becomes an `EVENTS` stub and never reaches the author.
     */
    #bindContext(ctx) {
      const source = ctx && typeof ctx === "object" ? ctx : {};
      const { eventsToken, ...rest } = source;
      if (typeof eventsToken === "string" && eventsToken && this.env.GRANTED) {
        rest.events = new GrantedEvents(this.env.GRANTED, {
          Authorization: `Bearer ${eventsToken}`,
        });
      }
      return rest;
    }

    async fetch() {
      return new Response(null, { status: 404 });
    }

    /**
     * `PluginDescribe`: the manifest projection the launcher binds as
     * `PLUGIN_DESCRIBE`, refined by the author's optional `describe()`.
     * Identity and capabilities always come from the manifest.
     */
    async describe() {
      const native = this.#native();
      if (native) return native.describe();
      const base = parseJsonBinding(this.env.PLUGIN_DESCRIBE) ?? {};
      const author = (await this.#author().bookclerkDescribe()) ?? {};
      return {
        ...base,
        ...author,
        apiVersion: base.apiVersion ?? author.apiVersion ?? PRODUCT_API_VERSION,
        id: base.id ?? author.id,
        capabilities: base.capabilities ?? author.capabilities,
      };
    }

    /** Call `method` on the named entrypoint `name` with the granted context. */
    async invokeEntrypoint(name, ctx, method, args = []) {
      const list = Array.isArray(args) ? args : [];
      const native = this.#native();
      if (native) return native.invoke(name, stripEventsToken(ctx), method, list);
      return this.#named(name).bookclerkInvoke(this.#bindContext(ctx), method, ...list);
    }

    /** Deliver one domain event to the default entrypoint's `event(batch)`. */
    async invokeEvent(ctx, event) {
      const native = this.#native();
      if (native) return native.event(stripEventsToken(ctx), event);
      const results = await this.#author().bookclerkEvent(this.#bindContext(ctx), {
        events: [event],
      });
      const first = Array.isArray(results) ? results[0] : undefined;
      if (!first || typeof first.kind !== "string") {
        throw PluginError.fromWire("internal", "event handler returned no result");
      }
      return first;
    }

    async invokeDestination(op, ctx, args = {}, body) {
      switch (op) {
        case "head":
          return this.invokeEntrypoint("storage", ctx, "head", [String(args.key ?? "")]);
        case "list":
          return this.invokeEntrypoint("storage", ctx, "list", [args.options ?? {}]);
        case "get":
          return this.invokeEntrypoint("storage", ctx, "get", [String(args.key ?? ""), args.options]);
        case "put": {
          if (!body) {
            throw PluginError.fromWire("invalid_params", "put missing body stream");
          }
          const options = args.options;
          const bounded = exactLengthBody(body, options?.contentLength);
          return this.invokeEntrypoint("storage", ctx, "put", [String(args.key ?? ""), bounded, options]);
        }
        case "copy":
          return this.invokeEntrypoint("storage", ctx, "copy", [String(args.from ?? ""), String(args.to ?? "")]);
        case "delete":
          await this.invokeEntrypoint("storage", ctx, "delete", [String(args.key ?? "")]);
          return { ok: true };
        case "commit":
          return this.invokeEntrypoint("storage", ctx, "commit", [
            String(args.key ?? ""),
            String(args.commitToken ?? ""),
          ]);
        case "abortStage":
          await this.invokeEntrypoint("storage", ctx, "abortStage", [
            String(args.key ?? ""),
            String(args.commitToken ?? ""),
          ]);
          return { ok: true };
        default:
          throw PluginError.fromWire("unsupported", `destination.${op}`);
      }
    }

    async invokeSourceOpen(ctx, key) {
      return this.invokeEntrypoint("storage", ctx, "get", [String(key ?? "")]);
    }

    async invokeHandle(ctx, invocation, grantToken, _databases) {
      if (this.#native()) {
        throw PluginError.fromWire("unsupported", "native job runner via broker not bound");
      }
      const controller = new AbortController();
      try {
        const granted = grantedJobCapabilities(this.env, grantToken, controller);
        return await this.#author().bookclerkJob(
          this.#bindContext(ctx),
          invocation ?? {},
          granted,
        );
      } finally {
        controller.abort();
      }
    }

    async cliDescribe() {
      return this.invokeEntrypoint("cli", {}, "describe", []);
    }

    async cliInvoke(params) {
      return this.invokeEntrypoint("cli", {}, "invoke", [params ?? {}]);
    }

    async oidcClients() {
      const clients = await this.invokeEntrypoint("oidc", {}, "clients", []);
      return Array.isArray(clients) ? clients : [];
    }

    async databaseMigrations(binding) {
      if (this.#native()) return [];
      return this.#author().bookclerkDatabaseMigrations(String(binding ?? ""));
    }

    async shutdown() {
      if (this.#native()) return;
      await this.#author().bookclerkShutdown();
    }
  };
}
