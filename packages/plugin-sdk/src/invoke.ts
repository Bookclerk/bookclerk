/**
 * `POST /invoke` dispatch for the trusted adapter isolate: decodes one
 * `<Interface>.<method>$Params` Cap'n Proto message, calls the author class
 * through its `bookclerk*` dispatch surface, and encodes the
 * `<Interface>.<method>$Results` reply. See `docs/internals/workerd-bridge.md`.
 *
 * Author failures ride inside the reply union (`result.err`); only transport
 * failures (unknown method, malformed bytes, missing binding) surface as
 * {@link InvokeError} with the HTTP status the bridge answers with.
 *
 * @internal
 * @module
 */

import type { CapTable } from "./db-capnp.js";
import { MAX_BATCH_KEYS, MAX_SCALAR_BYTES } from "./abi.js";
import { PluginError } from "./errors.js";
import type {
  AdapterDatabaseSession as WireAdapterDatabaseSession,
  AdapterExecuteRequest,
  AiModel,
  AiTextEmbeddingsOutput,
  CatalogHit,
  DbBootstrap,
  DbCapabilities,
  EventResult,
  ExecuteReply,
  ExecuteStreamReply,
  ExternalUser,
  IdentityHighWater,
  JobController as WireJobController,
  JobOutcome,
  ListeningProgress,
  CoverImage,
  MetadataFileRecord,
  MetadataRecord,
  KeyOutcome as WireKeyOutcome,
  ObjectMetadata as WireObjectMetadata,
  OidcClientTemplate,
  PluginError as WirePluginError,
  PluginMigration as WirePluginMigration,
  Provisioned,
  ProvisionedSecret,
  PurchaseHint,
  PurchaseHintResult,
  QueueResult,
  SocketMessageOk,
  SourceAccount,
} from "./generated.js";
import * as W from "./generated-wire.js";
import type {
  AuthorStub,
  BridgeContext,
  EntrypointName,
  EventOutcome,
  GrantedContext,
  CancelWatchLike,
  GrantedJobCapabilities,
  JobOutcomeRecord,
  ChangesRequest,
  KeyOutcome,
  ListOptions,
  MoveManyPage,
  MoveManyRequest,
  MoveOptions,
  NamedStub,
  ObjectMetadata,
  PluginMigration,
  PurchaseHintAnswer,
  QueueOutcome,
} from "./plugin.js";

/**
 * One `X-Bookclerk-Caps` entry: capability index `i` of the message is
 * described by entry `i`.
 *
 * @internal
 */
export interface CapDescriptor {
  /** `source` / `destination` / `progress` / `cancellation` / … / `adapterSession`. */
  kind: string;
  /** Grant token for host-served kinds. */
  token?: string;
  /** Binding name for `guestDatabase`. */
  binding?: string;
  /** Isolate object id for `adapterSession`. */
  id?: string;
}

/**
 * Successful `/invoke` reply: `$Results` bytes plus the descriptors of
 * capabilities the reply exported.
 *
 * @internal
 */
export interface InvokeReply {
  /** Unpacked single-segment Cap'n message (`<Interface>.<method>$Results`). */
  body: Uint8Array;
  /** Reply capability table (`X-Bookclerk-Caps` on the response). */
  caps: CapDescriptor[];
}

/**
 * Transport-level `/invoke` failure as a plain value (Workers RPC drops own
 * properties of thrown errors, so the adapter returns this instead of
 * throwing across the service binding).
 *
 * @internal
 */
export interface InvokeFailure {
  /** HTTP status the bridge answers with (`400` / `404` / `500`). */
  status: number;
  /** JSON error body. */
  error: { code: string; message: string };
}

/**
 * What the adapter's `invoke` resolves to.
 *
 * @internal
 */
export type InvokeOutcome = InvokeReply | InvokeFailure;

/**
 * Transport-level `/invoke` failure (never an author failure): unknown
 * interface/method, malformed message, bad capability table, or a missing
 * binding.
 *
 * @internal
 */
export class InvokeError extends PluginError {
  /** HTTP status the bridge answers with. */
  readonly status: number;

  constructor(status: number, code: string, message: string) {
    super(code, message);
    this.name = "InvokeError";
    this.status = status;
  }
}

/**
 * Adapter hooks the dispatcher calls back into.
 *
 * @internal
 */
export interface InvokeHost {
  /** Default author entrypoint (`PLUGIN`), or `undefined` when unbound. */
  author(): AuthorStub | undefined;
  /** Named author entrypoint, or `undefined` when the isolate does not export it. */
  named(name: EntrypointName): NamedStub | undefined;
  /** Bridge context → author-facing granted context. */
  bindContext(ctx: BridgeContext | undefined): GrantedContext;
  /**
   * Resolve one request capability descriptor; `undefined` for an unknown
   * kind. `stop` aborts when the call returns.
   */
  importCap(descriptor: CapDescriptor, stop: AbortSignal): unknown;
}

interface CallContext {
  host: InvokeHost;
  ctx: BridgeContext | undefined;
  target: string | null;
  stop: AbortController;
}

type Stub = AuthorStub | NamedStub;

interface MethodSpec<P, R> {
  params: W.StructCodec<P>;
  results: W.StructCodec<{ result: R }>;
  /** Named entrypoint, or `author` for the default `PLUGIN` stub. */
  stub: EntrypointName | "author";
  /** Calls the author and returns the `ok` payload of the reply union. */
  call(stub: Stub, params: P, cx: CallContext): Promise<unknown>;
  /** Wraps the `ok` payload as the reply union (`undefined` → `{ kind: "ok" }`). */
  ok(value: unknown): R;
}

type AnySpec = MethodSpec<unknown, unknown>;

function okValue<V>(value: V): { kind: "ok"; value: V } {
  return { kind: "ok", value };
}

// Typed structs the author returns pass through as the `ok` payload unchanged.
function okPass<R>(value: unknown): R {
  return { kind: "ok", value } as unknown as R;
}

function okEmpty(): { kind: "ok" } {
  return { kind: "ok" };
}

function named(stub: Stub, ctx: GrantedContext, method: string, args: unknown[]): Promise<unknown> {
  return (stub as NamedStub).bookclerkInvoke(ctx, method, ...args);
}

function spec<P, R>(entry: MethodSpec<P, R>): AnySpec {
  return entry as unknown as AnySpec;
}

// `{ params }` in, `[params]` to the author.
function withParams<P extends { params: unknown }, R>(
  stub: EntrypointName,
  method: string,
  params: W.StructCodec<P>,
  results: W.StructCodec<{ result: R }>,
  ok: (value: unknown) => R,
): AnySpec {
  return spec<P, R>({
    params,
    results,
    stub,
    call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), method, [p.params]),
    ok,
  });
}

// No params, `[]` to the author.
function withoutParams<P, R>(
  stub: EntrypointName,
  method: string,
  params: W.StructCodec<P>,
  results: W.StructCodec<{ result: R }>,
  ok: (value: unknown) => R,
): AnySpec {
  return spec<P, R>({
    params,
    results,
    stub,
    call: (s, _p, cx) => named(s, cx.host.bindContext(cx.ctx), method, []),
    ok,
  });
}

const asList = <T>(value: unknown): T[] => (Array.isArray(value) ? (value as T[]) : []);

/**
 * One `purchaseHint` answer as the wire's `PurchaseHintResult`: a bare
 * hint, `null`, or a {@link PurchaseHintAnswer} with title-search records.
 *
 * @param value - What the plugin's method returned for one query.
 * @returns `found`, the hint and any records.
 */
function hintResult(value: unknown): PurchaseHintResult {
  if (value != null && typeof value === "object" && "records" in value) {
    const answer = value as PurchaseHintAnswer;
    return {
      found: answer.hint != null,
      // The wire leaves `hint` unset when not found.
      hint: (answer.hint ?? undefined) as PurchaseHint,
      records: Array.isArray(answer.records) ? answer.records : [],
    };
  }
  return { found: value != null, hint: (value ?? undefined) as PurchaseHint, records: [] };
}

const okHits = (value: unknown) => okValue({ hits: asList<CatalogHit>(value) });
/**
 * Wraps a metadata provider's `lookup` / `search` return as `MetadataRecords`.
 * Anything but an array is an author bug, not an empty answer.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the records.
 * @throws {PluginError} `internal` when `value` is not an array.
 */
const okRecords = (value: unknown) => {
  if (!Array.isArray(value)) {
    throw PluginError.fromWire("internal", "metadata provider must return a MetadataRecord[]");
  }
  return okValue({ records: value as MetadataRecord[] });
};
/**
 * Wraps a cover fetcher's `fetchCovers` return as `CoverImages`. The Rust
 * client checks the count and URLs against the request.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the per-URL answers.
 * @throws {PluginError} `internal` when `value` is not an array.
 */
const okCoverImages = (value: unknown) => {
  if (!Array.isArray(value)) {
    throw PluginError.fromWire("internal", "cover fetcher must return a CoverImage[]");
  }
  return okValue({ images: value as CoverImage[] });
};
/**
 * Wraps a metadata-file dialect's `parse` return as `MetadataFileParsed`. The
 * Rust client checks the record against the ABI caps.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the record.
 * @throws {PluginError} `internal` when `value` is not an object with a
 *   `present` list.
 */
const okFileRecord = (value: unknown) => {
  if (
    value === null ||
    typeof value !== "object" ||
    !Array.isArray((value as { present?: unknown }).present)
  ) {
    throw PluginError.fromWire(
      "internal",
      "metadata file dialect must return a MetadataFileRecord",
    );
  }
  return okValue({ record: value as MetadataFileRecord });
};
/**
 * Wraps a metadata-file dialect's `render` return as `MetadataFileRendered`;
 * a string becomes its UTF-8 bytes.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the bytes.
 * @throws {PluginError} `internal` when `value` is neither bytes nor a string.
 */
const okFileBytes = (value: unknown) => {
  if (typeof value === "string") {
    return okValue({ bytes: new TextEncoder().encode(value) });
  }
  if (!(value instanceof Uint8Array)) {
    throw PluginError.fromWire(
      "internal",
      "metadata file dialect must render a Uint8Array or string",
    );
  }
  return okValue({ bytes: value });
};
/**
 * Wraps a model plugin's `models` return as `AiModelList`. The Rust client
 * checks every model.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the listing.
 * @throws {PluginError} `internal` when `value` has no `models` list.
 */
const okAiModels = (value: unknown) => {
  const v = (value ?? {}) as { models?: unknown };
  if (!Array.isArray(v.models)) {
    throw PluginError.fromWire("internal", "models must return an object with a models list");
  }
  return okValue({ models: v.models as AiModel[] });
};
/**
 * Wraps a model plugin's `run` return as `AiRunOk`. The host checks the
 * output against the model's listing.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the output.
 * @throws {PluginError} `internal` when `value` has no text-embedding output
 *   with `vectors` bytes.
 */
// A guided setup's answer, with absent lists and settings filled in.
const okProvisioned = (value: unknown) => {
  const out = (value ?? {}) as Partial<Provisioned>;
  return okValue({
    settingsJson: typeof out.settingsJson === "string" ? out.settingsJson : "{}",
    secrets: asList<ProvisionedSecret>(out.secrets).map((secret) => ({
      key: String(secret.key ?? ""),
      value: String(secret.value ?? ""),
    })),
    summary: asList<string>(out.summary).map(String),
  });
};

const okAiRun = (value: unknown) => {
  const out = ((value ?? {}) as { output?: { textEmbeddings?: AiTextEmbeddingsOutput } }).output
    ?.textEmbeddings;
  if (!out || !(out.vectors instanceof Uint8Array)) {
    throw PluginError.fromWire(
      "internal",
      "run must return output.textEmbeddings with vectors as a Uint8Array",
    );
  }
  return okValue({
    output: {
      textEmbeddings: {
        dimensions: Number(out.dimensions),
        vectors: out.vectors,
        revision: String(out.revision ?? ""),
        fallbacks: Array.isArray(out.fallbacks) ? out.fallbacks.map(Number) : [],
      },
    },
  });
};
/**
 * Wraps a notifier's `send` return as `NotifySent`; `undefined` means the
 * service gave no message id.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the provider id, if any.
 */
const okNotifySent = (value: unknown) => {
  const v = (value ?? {}) as { providerId?: unknown };
  const providerId =
    v.providerId === undefined || v.providerId === null ? "" : String(v.providerId);
  return okValue(providerId ? { providerId } : {});
};
/**
 * Wraps a notifier's `validateDestination` return as `NotifyValidated`. The
 * Rust client checks the address document and label.
 *
 * @param value - The author method's return value.
 * @returns The `ok` arm carrying the normalised address and label.
 * @throws {PluginError} `internal` when `value` has no string `addressJson`.
 */
const okNotifyValidated = (value: unknown) => {
  const v = (value ?? {}) as { addressJson?: unknown; label?: unknown };
  if (typeof v.addressJson !== "string") {
    throw PluginError.fromWire(
      "internal",
      "validateDestination must return an object with an addressJson string",
    );
  }
  const label = typeof v.label === "string" ? v.label : "";
  return okValue(label ? { addressJson: v.addressJson, label } : { addressJson: v.addressJson });
};
const okLines = (value: unknown) =>
  okValue({ lines: asList<unknown>(value).map((l) => String(l)) });
const okHealth = (value: unknown) => {
  const v = (value ?? {}) as { ok?: unknown; detail?: unknown };
  return okValue({ ok: Boolean(v.ok), detail: String(v.detail ?? "") });
};

function queueResult(outcome: QueueOutcome): QueueResult {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack" };
    case "retry":
      return {
        kind: "retry",
        value: {
          delaySeconds: Number(outcome.delaySeconds) || 0,
          reason: String(outcome.reason ?? ""),
        },
      };
    default:
      return { kind: "retry", value: { delaySeconds: 0, reason: "unknown queue outcome" } };
  }
}

function eventResult(outcome: EventOutcome): EventResult {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack", value: { dummy: undefined } };
    case "retry":
      return {
        kind: "retry",
        value: {
          retryAtUnixMs: Number(outcome.retryAtUnixMs) || 0,
          reason: String(outcome.reason ?? ""),
        },
      };
    case "reject":
      return { kind: "reject", value: { reason: String(outcome.reason ?? "") } };
    case "deadLetter":
      return { kind: "deadLetter", value: { reason: String(outcome.reason ?? "") } };
    case "suspended":
      return {
        kind: "suspended",
        value: {
          checkpointJson: String(outcome.checkpointJson ?? ""),
          checkpointSchemaVersion: Number(outcome.checkpointSchemaVersion) || 0,
          wakeAtUnixMs: Number(outcome.wakeAtUnixMs) || 0,
          wakeOnEventType: String(outcome.wakeOnEventType ?? ""),
          wakeOnFilterJson: String(outcome.wakeOnFilterJson ?? ""),
        },
      };
    default:
      throw PluginError.fromWire(
        "internal",
        `unknown event outcome ${String((outcome as { kind: unknown }).kind)}`,
      );
  }
}

function jobOutcome(record: JobOutcomeRecord): JobOutcome {
  switch (record.kind) {
    case "completed":
      return {
        kind: "completed",
        value: {
          message: String(record.message ?? ""),
          bytesCopied: Number(record.bytesCopied) || 0,
        },
      };
    case "retryable":
      return {
        kind: "retryable",
        value: {
          message: String(record.message ?? ""),
          retryAfterUnixMs: Number(record.retryAfterUnixMs) || 0,
        },
      };
    case "rejected":
      return { kind: "rejected", value: { message: String(record.message ?? "") } };
    case "cancelled":
      return { kind: "cancelled", value: { message: String(record.message ?? "") } };
    case "suspended":
      return {
        kind: "suspended",
        value: {
          checkpointJson: String(record.checkpoint?.json ?? ""),
          checkpointSchemaVersion: Number(record.checkpoint?.schemaVersion) || 0,
          wakeAtUnixMs: Number(record.wakeAtUnixMs) || 0,
        },
      };
    default:
      throw PluginError.fromWire(
        "internal",
        `unknown job outcome ${String((record as { kind: unknown }).kind)}`,
      );
  }
}

function wireMigration(migration: PluginMigration): WirePluginMigration {
  const ops = Array.isArray(migration?.operations) ? migration.operations : [];
  return {
    id: String(migration?.id ?? ""),
    operations: ops.map((op) => {
      const o = (op ?? {}) as { kind?: unknown; value?: unknown; schema?: unknown; data?: unknown };
      if (typeof o.schema === "string") return { kind: "schema", value: o.schema };
      if (typeof o.data === "string") return { kind: "data", value: o.data };
      if (o.kind === "schema" || o.kind === "data")
        return { kind: o.kind, value: String(o.value ?? "") };
      throw PluginError.fromWire(
        "invalid_params",
        `migration \`${String(migration?.id)}\` has an op without schema/data`,
      );
    }),
  };
}

/**
 * Maps wire listing options to the author's, turning each zero value (the
 * wire's "absent") into `undefined`, hints included.
 *
 * @param options - Decoded wire options, or `undefined` when none were sent.
 * @returns The options handed to `Destination.list`.
 */
function listOptions(
  options: Partial<W.DestinationListParams["options"]> | undefined,
): ListOptions {
  return {
    prefix: options?.prefix || undefined,
    cursor: options?.cursor || undefined,
    limit: options?.limit || undefined,
    withSignatures: options?.withSignatures || undefined,
    modifiedSinceMs: options?.modifiedSinceMs || undefined,
  };
}

/**
 * Maps wire move options to the author's; `false` and empty text become
 * `undefined`.
 *
 * @param options - Decoded wire options, or `undefined` when none were sent.
 * @returns The options handed to `move` and `moveMany`.
 */
function moveOptions(
  options: Partial<W.DestinationMoveParams["options"]> | undefined,
): MoveOptions {
  return {
    noOverwrite: options?.noOverwrite || undefined,
    ifSourceMatch: options?.ifSourceMatch || undefined,
  };
}

/**
 * Maps a wire `moveMany` request to the author's, refusing more than
 * `MAX_BATCH_KEYS` explicit pairs. A prefix target is not counted: its page
 * size is the author's `limit`.
 *
 * @param request - Decoded wire request, or `undefined` when none was sent.
 * @returns The request handed to `moveMany`.
 * @throws PluginError `payload_too_large` above the pair cap.
 */
function moveManyRequest(
  request: Partial<W.DestinationMoveManyParams["request"]> | undefined,
): MoveManyRequest {
  const target = request?.target ?? { kind: "pairs", value: [] };
  if (target.kind === "pairs" && target.value.length > MAX_BATCH_KEYS) {
    throw new PluginError("payload_too_large", `moveMany of ${target.value.length} pairs`);
  }
  return {
    target,
    cursor: request?.cursor || undefined,
    limit: request?.limit || undefined,
    options: moveOptions(request?.options),
  };
}

/**
 * Checks a batch call's key list before the author's handler sees it. The
 * host never sends an empty list or more than `MAX_BATCH_KEYS` keys.
 *
 * @param keys - Decoded wire keys, or `undefined` when none were sent.
 * @returns The keys as strings, in the order sent.
 * @throws PluginError `invalid_params` for no keys, `payload_too_large`
 *   above the cap.
 */
function batchKeys(keys: string[] | undefined): string[] {
  const list = keys ?? [];
  if (list.length === 0) {
    throw new PluginError("invalid_params", "a batch call needs at least one key");
  }
  if (list.length > MAX_BATCH_KEYS) {
    throw new PluginError("payload_too_large", `batch of ${list.length} keys`);
  }
  return list.map(String);
}

/**
 * Maps the author's per-key outcomes to the wire's.
 *
 * An author outcome without `error` is a success and goes out with an empty
 * wire code, which is how the wire spells success. An author outcome with
 * `error` is a failure even when its code is empty, so an empty code goes
 * out as `internal` rather than as success.
 *
 * @param outcomes - The author's outcomes, one per key.
 * @returns Wire outcomes in the same order.
 */
function wireOutcomes(outcomes: KeyOutcome[]): WireKeyOutcome[] {
  return outcomes.map((o) => ({
    key: String(o.key ?? ""),
    error: {
      code: o.error ? String(o.error.code ?? "") || "internal" : "",
      message: o.error ? String(o.error.message ?? "") : "",
      details: "",
    },
  }));
}

function sessionCall(method: string): AnySpec["call"] {
  return (s, p, cx) => {
    if (!cx.target) {
      throw new InvokeError(
        400,
        "invalid_params",
        `AdapterDatabaseSession.${method} requires X-Bookclerk-Target`,
      );
    }
    const params = p as Record<string, unknown>;
    const args =
      method === "execute" || method === "executeStreamed"
        ? [params.request as AdapterExecuteRequest]
        : method === "importIdentity"
          ? [params.rows as IdentityHighWater[]]
          : method === "dropUserRelations"
            ? [params.names as string[]]
            : [];
    return named(s, cx.host.bindContext(cx.ctx), "session", [cx.target, method, ...args]);
  };
}

function session<P, R>(
  method: string,
  params: W.StructCodec<P>,
  results: W.StructCodec<{ result: R }>,
  ok: (value: unknown) => R,
): AnySpec {
  return spec<P, R>({ params, results, stub: "databaseAdapter", call: sessionCall(method), ok });
}

const METHODS: Readonly<Record<string, Readonly<Record<string, AnySpec>>>> = {
  PluginWorker: {
    databaseMigrations: spec<
      W.PluginWorkerDatabaseMigrationsParams,
      W.PluginWorkerDatabaseMigrationsResults["result"]
    >({
      params: W.PluginWorkerDatabaseMigrationsParamsCodec,
      results: W.PluginWorkerDatabaseMigrationsResultsCodec,
      stub: "author",
      call: (s, p) => (s as AuthorStub).bookclerkDatabaseMigrations(String(p.binding ?? "")),
      ok: (value) => okValue({ migrations: asList<PluginMigration>(value).map(wireMigration) }),
    }),
  },
  ContentSource: {
    login: withParams(
      "storefront",
      "login",
      W.ContentSourceLoginParamsCodec,
      W.ContentSourceLoginResultsCodec,
      okPass,
    ),
    scan: withParams(
      "storefront",
      "scan",
      W.ContentSourceScanParamsCodec,
      W.ContentSourceScanResultsCodec,
      okPass,
    ),
    fetchTitle: withParams(
      "storefront",
      "fetchTitle",
      W.ContentSourceFetchTitleParamsCodec,
      W.ContentSourceFetchTitleResultsCodec,
      okPass,
    ),
    listAccounts: withoutParams(
      "storefront",
      "listAccounts",
      W.ContentSourceListAccountsParamsCodec,
      W.ContentSourceListAccountsResultsCodec,
      (value) => okValue({ accounts: asList<SourceAccount>(value) }),
    ),
    loginStart: withParams(
      "storefront",
      "loginStart",
      W.ContentSourceLoginStartParamsCodec,
      W.ContentSourceLoginStartResultsCodec,
      okPass,
    ),
    loginComplete: withParams(
      "storefront",
      "loginComplete",
      W.ContentSourceLoginCompleteParamsCodec,
      W.ContentSourceLoginCompleteResultsCodec,
      okPass,
    ),
    searchCatalog: withParams(
      "storefront",
      "searchCatalog",
      W.ContentSourceSearchCatalogParamsCodec,
      W.ContentSourceSearchCatalogResultsCodec,
      okHits,
    ),
    expandCandidates: withParams(
      "storefront",
      "expandCandidates",
      W.ContentSourceExpandCandidatesParamsCodec,
      W.ContentSourceExpandCandidatesResultsCodec,
      okHits,
    ),
    purchaseHint: withParams(
      "storefront",
      "purchaseHint",
      W.ContentSourcePurchaseHintParamsCodec,
      W.ContentSourcePurchaseHintResultsCodec,
      (value) => okValue(hintResult(value)),
    ),
    purchaseHints: withParams(
      "storefront",
      "purchaseHints",
      W.ContentSourcePurchaseHintsParamsCodec,
      W.ContentSourcePurchaseHintsResultsCodec,
      (value) =>
        okValue({
          results: asList<PurchaseHint | PurchaseHintAnswer | null>(value).map(hintResult),
        }),
    ),
    listDeals: withParams(
      "storefront",
      "listDeals",
      W.ContentSourceListDealsParamsCodec,
      W.ContentSourceListDealsResultsCodec,
      okHits,
    ),
    health: withoutParams(
      "storefront",
      "health",
      W.ContentSourceHealthParamsCodec,
      W.ContentSourceHealthResultsCodec,
      okHealth,
    ),
    diagnose: withoutParams(
      "storefront",
      "diagnose",
      W.ContentSourceDiagnoseParamsCodec,
      W.ContentSourceDiagnoseResultsCodec,
      okLines,
    ),
    catalogDetail: withParams(
      "storefront",
      "catalogDetail",
      W.ContentSourceCatalogDetailParamsCodec,
      W.ContentSourceCatalogDetailResultsCodec,
      (value) => okValue({ found: value != null, hit: (value ?? undefined) as CatalogHit }),
    ),
    revokeAccount: withParams(
      "storefront",
      "revokeAccount",
      W.ContentSourceRevokeAccountParamsCodec,
      W.ContentSourceRevokeAccountResultsCodec,
      okEmpty,
    ),
    qualityOffers: withParams(
      "storefront",
      "qualityOffers",
      W.ContentSourceQualityOffersParamsCodec,
      W.ContentSourceQualityOffersResultsCodec,
      okPass,
    ),
  },
  Destination: {
    head: spec<W.DestinationHeadParams, W.DestinationHeadResults["result"]>({
      params: W.DestinationHeadParamsCodec,
      results: W.DestinationHeadResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "head", [String(p.key ?? "")]),
      ok: (value) => {
        const meta = (value ?? null) as ObjectMetadata | null;
        return okValue({
          found: meta != null,
          meta: (meta ?? undefined) as unknown as WireObjectMetadata,
        });
      },
    }),
    list: spec<W.DestinationListParams, W.DestinationListResults["result"]>({
      params: W.DestinationListParamsCodec,
      results: W.DestinationListResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "list", [listOptions(p.options)]),
      ok: okPass,
    }),
    copy: spec<W.DestinationCopyParams, W.DestinationCopyResults["result"]>({
      params: W.DestinationCopyParamsCodec,
      results: W.DestinationCopyResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "copy", [String(p.from ?? ""), String(p.to ?? "")]),
      ok: okPass,
    }),
    delete: spec<W.DestinationDeleteParams, W.DestinationDeleteResults["result"]>({
      params: W.DestinationDeleteParamsCodec,
      results: W.DestinationDeleteResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "delete", [String(p.key ?? "")]),
      ok: okEmpty,
    }),
    commit: spec<W.DestinationCommitParams, W.DestinationCommitResults["result"]>({
      params: W.DestinationCommitParamsCodec,
      results: W.DestinationCommitResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "commit", [
          String(p.key ?? ""),
          String(p.commitToken ?? ""),
        ]),
      ok: okPass,
    }),
    abortStage: spec<W.DestinationAbortStageParams, W.DestinationAbortStageResults["result"]>({
      params: W.DestinationAbortStageParamsCodec,
      results: W.DestinationAbortStageResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "abortStage", [
          String(p.key ?? ""),
          String(p.commitToken ?? ""),
        ]),
      ok: okEmpty,
    }),
    move: spec<W.DestinationMoveParams, W.DestinationMoveResults["result"]>({
      params: W.DestinationMoveParamsCodec,
      results: W.DestinationMoveResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "move", [
          String(p.from ?? ""),
          String(p.to ?? ""),
          moveOptions(p.options),
        ]),
      ok: okPass,
    }),
    moveMany: spec<W.DestinationMoveManyParams, W.DestinationMoveManyResults["result"]>({
      params: W.DestinationMoveManyParamsCodec,
      results: W.DestinationMoveManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "moveMany", [moveManyRequest(p.request)]),
      ok: (value) => {
        const page = (value ?? { results: [] }) as MoveManyPage;
        return okValue({
          results: wireOutcomes(asList<KeyOutcome>(page.results)),
          nextCursor: page.nextCursor ?? "",
          atomic: page.atomic ?? false,
          objectsMoved: page.objectsMoved ?? 0,
        });
      },
    }),
    deleteMany: spec<W.DestinationDeleteManyParams, W.DestinationDeleteManyResults["result"]>({
      params: W.DestinationDeleteManyParamsCodec,
      results: W.DestinationDeleteManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "deleteMany", [batchKeys(p.keys)]),
      ok: (value) => okValue({ results: wireOutcomes(asList<KeyOutcome>(value)) }),
    }),
    headMany: spec<W.DestinationHeadManyParams, W.DestinationHeadManyResults["result"]>({
      params: W.DestinationHeadManyParamsCodec,
      results: W.DestinationHeadManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "headMany", [batchKeys(p.keys)]),
      ok: (value) =>
        okValue({
          objects: asList<ObjectMetadata | null>(value).map((meta) => ({
            found: meta != null,
            meta: (meta ?? undefined) as unknown as WireObjectMetadata,
          })),
        }),
    }),
    translateKeys: spec<
      W.DestinationTranslateKeysParams,
      W.DestinationTranslateKeysResults["result"]
    >({
      params: W.DestinationTranslateKeysParamsCodec,
      results: W.DestinationTranslateKeysResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "translateKeys", [batchKeys(p.keys)]),
      ok: (value) => okValue({ keys: asList<unknown>(value).map(String) }),
    }),
    changes: spec<W.DestinationChangesParams, W.DestinationChangesResults["result"]>({
      params: W.DestinationChangesParamsCodec,
      results: W.DestinationChangesResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "changes", [
          {
            cursor: p.request?.cursor || undefined,
            limit: p.request?.limit || undefined,
          } satisfies ChangesRequest,
        ]),
      ok: okPass,
    }),
  },
  RemoteLibrary: {
    health: withoutParams(
      "remoteLibrary",
      "health",
      W.RemoteLibraryHealthParamsCodec,
      W.RemoteLibraryHealthResultsCodec,
      okHealth,
    ),
    start: withoutParams(
      "remoteLibrary",
      "start",
      W.RemoteLibraryStartParamsCodec,
      W.RemoteLibraryStartResultsCodec,
      okEmpty,
    ),
    stop: withoutParams(
      "remoteLibrary",
      "stop",
      W.RemoteLibraryStopParamsCodec,
      W.RemoteLibraryStopResultsCodec,
      okEmpty,
    ),
    diagnose: withoutParams(
      "remoteLibrary",
      "diagnose",
      W.RemoteLibraryDiagnoseParamsCodec,
      W.RemoteLibraryDiagnoseResultsCodec,
      okLines,
    ),
    scanLibrary: withParams(
      "remoteLibrary",
      "scanLibrary",
      W.RemoteLibraryScanLibraryParamsCodec,
      W.RemoteLibraryScanLibraryResultsCodec,
      okEmpty,
    ),
    syncListening: withoutParams(
      "remoteLibrary",
      "syncListening",
      W.RemoteLibrarySyncListeningParamsCodec,
      W.RemoteLibrarySyncListeningResultsCodec,
      (value) => okValue({ items: asList<ListeningProgress>(value) }),
    ),
    pollEvents: withoutParams(
      "remoteLibrary",
      "pollEvents",
      W.RemoteLibraryPollEventsParamsCodec,
      W.RemoteLibraryPollEventsResultsCodec,
      (value) => okValue({ users: asList<ExternalUser>(value) }),
    ),
  },
  EventConsumer: {
    event: spec<W.EventConsumerEventParams, W.EventConsumerEventResults["result"]>({
      params: W.EventConsumerEventParamsCodec,
      results: W.EventConsumerEventResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkEvent(cx.host.bindContext(cx.ctx), {
          events: Array.isArray(p.batch?.events) ? p.batch.events : [],
        }),
      ok: (value) => okValue(asList<EventOutcome>(value).map(eventResult)),
    }),
  },
  QueueConsumer: {
    queue: spec<W.QueueConsumerQueueParams, W.QueueConsumerQueueResults["result"]>({
      params: W.QueueConsumerQueueParamsCodec,
      results: W.QueueConsumerQueueResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkQueue(cx.host.bindContext(cx.ctx), {
          queue: String(p.batch?.queue ?? ""),
          messages: Array.isArray(p.batch?.messages) ? p.batch.messages : [],
        }),
      ok: (value) => okValue({ results: asList<QueueOutcome>(value).map(queueResult) }),
    }),
  },
  ScheduledHandler: {
    scheduled: spec<
      W.ScheduledHandlerScheduledParams,
      W.ScheduledHandlerScheduledResults["result"]
    >({
      params: W.ScheduledHandlerScheduledParamsCodec,
      results: W.ScheduledHandlerScheduledResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkScheduled(
          cx.host.bindContext(cx.ctx),
          p.controller ?? {},
          (p.cancel ?? null) as CancelWatchLike | null,
        ),
      ok: okEmpty,
    }),
  },
  SocketHandler: {
    message: spec<W.SocketHandlerMessageParams, W.SocketHandlerMessageResults["result"]>({
      params: W.SocketHandlerMessageParamsCodec,
      results: W.SocketHandlerMessageResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkSocketMessage(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: (value) =>
        okValue({
          pollEvents: (value as Partial<SocketMessageOk> | undefined)?.pollEvents === true,
        }),
    }),
    closed: spec<W.SocketHandlerClosedParams, W.SocketHandlerClosedResults["result"]>({
      params: W.SocketHandlerClosedParamsCodec,
      results: W.SocketHandlerClosedResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkSocketClosed(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: okEmpty,
    }),
  },
  WebhookHandler: {
    delivered: spec<W.WebhookHandlerDeliveredParams, W.WebhookHandlerDeliveredResults["result"]>({
      params: W.WebhookHandlerDeliveredParamsCodec,
      results: W.WebhookHandlerDeliveredResultsCodec,
      stub: "author",
      call: (s, p, cx) =>
        (s as AuthorStub).bookclerkWebhookDelivered(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: okEmpty,
    }),
  },
  JobRunner: {
    job: spec<W.JobRunnerJobParams, W.JobRunnerJobResults["result"]>({
      params: W.JobRunnerJobParamsCodec,
      results: W.JobRunnerJobResultsCodec,
      stub: "author",
      call: (s, p, cx) => {
        const controller = (p.controller ?? {}) as Partial<WireJobController>;
        const granted: GrantedJobCapabilities = {
          input: (controller.input ?? null) as GrantedJobCapabilities["input"],
          output: (controller.output ?? null) as GrantedJobCapabilities["output"],
          progress: (controller.progress ?? null) as GrantedJobCapabilities["progress"],
          cancel: (controller.cancel ?? null) as GrantedJobCapabilities["cancel"],
        };
        return (s as AuthorStub).bookclerkJob(
          cx.host.bindContext(cx.ctx),
          controller.invocation ?? {},
          granted,
        );
      },
      ok: (value) => okValue(jobOutcome(value as JobOutcomeRecord)),
    }),
  },
  PluginCli: {
    describe: withoutParams(
      "cli",
      "describe",
      W.PluginCliDescribeParamsCodec,
      W.PluginCliDescribeResultsCodec,
      okPass,
    ),
    invoke: withParams(
      "cli",
      "invoke",
      W.PluginCliInvokeParamsCodec,
      W.PluginCliInvokeResultsCodec,
      okPass,
    ),
  },
  Oidc: {
    clients: withoutParams(
      "oidc",
      "clients",
      W.OidcClientsParamsCodec,
      W.OidcClientsResultsCodec,
      (value) => okValue({ clients: asList<OidcClientTemplate>(value) }),
    ),
    authenticateUser: withParams(
      "oidc",
      "authenticateUser",
      W.OidcAuthenticateUserParamsCodec,
      W.OidcAuthenticateUserResultsCodec,
      okPass,
    ),
  },
  MetadataProvider: {
    health: withoutParams(
      "metadata",
      "health",
      W.MetadataProviderHealthParamsCodec,
      W.MetadataProviderHealthResultsCodec,
      okHealth,
    ),
    lookup: withParams(
      "metadata",
      "lookup",
      W.MetadataProviderLookupParamsCodec,
      W.MetadataProviderLookupResultsCodec,
      okRecords,
    ),
    search: withParams(
      "metadata",
      "search",
      W.MetadataProviderSearchParamsCodec,
      W.MetadataProviderSearchResultsCodec,
      okRecords,
    ),
    chapters: withParams(
      "metadata",
      "chapters",
      W.MetadataProviderChaptersParamsCodec,
      W.MetadataProviderChaptersResultsCodec,
      okPass,
    ),
  },
  CoverFetcher: {
    health: withoutParams(
      "covers",
      "health",
      W.CoverFetcherHealthParamsCodec,
      W.CoverFetcherHealthResultsCodec,
      okHealth,
    ),
    fetchCovers: spec<W.CoverFetcherFetchCoversParams, W.CoverFetcherFetchCoversResults["result"]>({
      params: W.CoverFetcherFetchCoversParamsCodec,
      results: W.CoverFetcherFetchCoversResultsCodec,
      stub: "covers",
      call: (s, p, cx) => {
        if (!p.sink) {
          throw PluginError.fromWire("invalid_params", "fetchCovers needs a sink");
        }
        return named(s, cx.host.bindContext(cx.ctx), "fetchCovers", [p.params, p.sink]);
      },
      ok: okCoverImages,
    }),
  },
  AiModels: {
    health: withoutParams(
      "ai",
      "health",
      W.AiModelsHealthParamsCodec,
      W.AiModelsHealthResultsCodec,
      okHealth,
    ),
    models: withoutParams(
      "ai",
      "models",
      W.AiModelsModelsParamsCodec,
      W.AiModelsModelsResultsCodec,
      okAiModels,
    ),
    run: withParams("ai", "run", W.AiModelsRunParamsCodec, W.AiModelsRunResultsCodec, okAiRun),
  },
  Provisioning: {
    provision: spec<W.ProvisioningProvisionParams, W.ProvisioningProvisionResults["result"]>({
      params: W.ProvisioningProvisionParamsCodec,
      results: W.ProvisioningProvisionResultsCodec,
      stub: "provisioning",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "provision", [p.request ?? {}]),
      ok: okProvisioned,
    }),
    deprovision: spec<W.ProvisioningDeprovisionParams, W.ProvisioningDeprovisionResults["result"]>({
      params: W.ProvisioningDeprovisionParamsCodec,
      results: W.ProvisioningDeprovisionResultsCodec,
      stub: "provisioning",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "deprovision", [p.request ?? {}]),
      ok: okEmpty,
    }),
  },
  Notifier: {
    health: withoutParams(
      "notifier",
      "health",
      W.NotifierHealthParamsCodec,
      W.NotifierHealthResultsCodec,
      okHealth,
    ),
    send: withParams(
      "notifier",
      "send",
      W.NotifierSendParamsCodec,
      W.NotifierSendResultsCodec,
      okNotifySent,
    ),
    validateDestination: withParams(
      "notifier",
      "validateDestination",
      W.NotifierValidateDestinationParamsCodec,
      W.NotifierValidateDestinationResultsCodec,
      okNotifyValidated,
    ),
  },
  MetadataFiles: {
    health: withoutParams(
      "metadataFiles",
      "health",
      W.MetadataFilesHealthParamsCodec,
      W.MetadataFilesHealthResultsCodec,
      okHealth,
    ),
    parse: withParams(
      "metadataFiles",
      "parse",
      W.MetadataFilesParseParamsCodec,
      W.MetadataFilesParseResultsCodec,
      okFileRecord,
    ),
    render: withParams(
      "metadataFiles",
      "render",
      W.MetadataFilesRenderParamsCodec,
      W.MetadataFilesRenderResultsCodec,
      okFileBytes,
    ),
  },
  Ingress: {
    start: withoutParams(
      "ingress",
      "start",
      W.IngressStartParamsCodec,
      W.IngressStartResultsCodec,
      okPass,
    ),
    health: withoutParams(
      "ingress",
      "health",
      W.IngressHealthParamsCodec,
      W.IngressHealthResultsCodec,
      okPass,
    ),
    stop: withoutParams(
      "ingress",
      "stop",
      W.IngressStopParamsCodec,
      W.IngressStopResultsCodec,
      okEmpty,
    ),
  },
  Database: {
    openSession: spec<W.DatabaseOpenSessionParams, W.DatabaseOpenSessionResults["result"]>({
      params: W.DatabaseOpenSessionParamsCodec,
      results: W.DatabaseOpenSessionResultsCodec,
      stub: "databaseAdapter",
      call: async (s, _p, cx) => {
        const id = await named(s, cx.host.bindContext(cx.ctx), "openSession", []);
        if (typeof id !== "string" || !id) {
          throw PluginError.fromWire("internal", "openSession returned no session id");
        }
        return id;
      },
      // The session id is exported through the reply CapTable as `adapterSession`.
      ok: (value) => okValue(value as unknown as WireAdapterDatabaseSession),
    }),
    dropUnit: spec<W.DatabaseDropUnitParams, W.DatabaseDropUnitResults["result"]>({
      params: W.DatabaseDropUnitParamsCodec,
      results: W.DatabaseDropUnitResultsCodec,
      stub: "databaseAdapter",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "dropUnit", [String(p.unitRef ?? "")]),
      ok: okEmpty,
    }),
  },
  AdapterDatabaseSession: {
    // An isolate adapter's streamed frames travel inside one invoke reply,
    // so a streamed budget above one scalar could not be delivered; the
    // advertisement is clamped so the host's budget is one the adapter can
    // honour before it commits.
    capabilities: session(
      "capabilities",
      W.AdapterDatabaseSessionCapabilitiesParamsCodec,
      W.AdapterDatabaseSessionCapabilitiesResultsCodec,
      (value) => okValue(clampIsolateStreamedCaps(value as DbCapabilities)),
    ),
    execute: session(
      "execute",
      W.AdapterDatabaseSessionExecuteParamsCodec,
      W.AdapterDatabaseSessionExecuteResultsCodec,
      (value) => okValue(value as ExecuteReply),
    ),
    close: session(
      "close",
      W.AdapterDatabaseSessionCloseParamsCodec,
      W.AdapterDatabaseSessionCloseResultsCodec,
      okEmpty,
    ),
    bootstrap: session(
      "bootstrap",
      W.AdapterDatabaseSessionBootstrapParamsCodec,
      W.AdapterDatabaseSessionBootstrapResultsCodec,
      (value) => okValue(value as DbBootstrap),
    ),
    exportIdentity: session(
      "exportIdentity",
      W.AdapterDatabaseSessionExportIdentityParamsCodec,
      W.AdapterDatabaseSessionExportIdentityResultsCodec,
      (value) => okValue(asList<IdentityHighWater>(value)),
    ),
    importIdentity: session(
      "importIdentity",
      W.AdapterDatabaseSessionImportIdentityParamsCodec,
      W.AdapterDatabaseSessionImportIdentityResultsCodec,
      okEmpty,
    ),
    listUserRelations: session(
      "listUserRelations",
      W.AdapterDatabaseSessionListUserRelationsParamsCodec,
      W.AdapterDatabaseSessionListUserRelationsResultsCodec,
      (value) => okValue(asList<unknown>(value).map((n) => String(n))),
    ),
    prepareUnitRestore: session(
      "prepareUnitRestore",
      W.AdapterDatabaseSessionPrepareUnitRestoreParamsCodec,
      W.AdapterDatabaseSessionPrepareUnitRestoreResultsCodec,
      okEmpty,
    ),
    dropUserRelations: session(
      "dropUserRelations",
      W.AdapterDatabaseSessionDropUserRelationsParamsCodec,
      W.AdapterDatabaseSessionDropUserRelationsResultsCodec,
      okEmpty,
    ),
    assertRestoreConstraints: session(
      "assertRestoreConstraints",
      W.AdapterDatabaseSessionAssertRestoreConstraintsParamsCodec,
      W.AdapterDatabaseSessionAssertRestoreConstraintsResultsCodec,
      okEmpty,
    ),
    // An isolate adapter answers with the frames in memory, which travel as
    // the `inline` member of the invoke reply; frames past one scalar cannot
    // cross that reply, so they become a typed limit error instead of a
    // transport failure.
    executeStreamed: session(
      "executeStreamed",
      W.AdapterDatabaseSessionExecuteStreamedParamsCodec,
      W.AdapterDatabaseSessionExecuteStreamedResultsCodec,
      (value) => inlineStreamReply(value),
    ),
  },
};

const CAP_KINDS: ReadonlySet<string> = new Set([
  "source",
  "destination",
  "progress",
  "cancellation",
]);

function requestCapTable(caps: CapDescriptor[], host: InvokeHost, stop: AbortSignal): CapTable {
  const resolved = new Map<number, unknown>();
  return {
    exportCap(): number {
      throw new InvokeError(400, "invalid_params", "request message cannot export capabilities");
    },
    importCap(index: number | null): unknown {
      if (index === null) return null;
      if (resolved.has(index)) return resolved.get(index);
      const descriptor = caps[index];
      if (!descriptor || typeof descriptor !== "object" || typeof descriptor.kind !== "string") {
        throw new InvokeError(400, "invalid_params", `capability index ${index} has no descriptor`);
      }
      if (!CAP_KINDS.has(descriptor.kind)) {
        throw new InvokeError(
          400,
          "invalid_params",
          `unsupported request capability kind ${descriptor.kind}`,
        );
      }
      const value = host.importCap(descriptor, stop);
      if (value === undefined) {
        throw new InvokeError(
          400,
          "invalid_params",
          `unsupported request capability kind ${descriptor.kind}`,
        );
      }
      resolved.set(index, value);
      return value;
    },
  };
}

function replyCapTable(caps: CapDescriptor[]): CapTable {
  return {
    exportCap(value: unknown): number {
      if (typeof value !== "string" || !value) {
        throw PluginError.fromWire("internal", "reply capability is not an isolate object id");
      }
      caps.push({ kind: "adapterSession", id: value });
      return caps.length - 1;
    },
    importCap(): unknown {
      throw new InvokeError(500, "internal", "reply message cannot import capabilities");
    },
  };
}

/**
 * Clamps an isolate adapter's streamed result budget to one scalar.
 *
 * The frames of an isolate adapter cross the invoke reply as a single
 * scalar, so a larger advertised budget would only fail after the batch
 * committed. With the clamp, the adapter's own result cap and the host's
 * budget agree on what can be delivered.
 *
 * @param caps - The adapter's advertised capabilities.
 * @returns The same capabilities with `maxStreamedResultBytes` at most
 *   `MAX_SCALAR_BYTES` (a `0`, no streaming, is kept).
 */
function clampIsolateStreamedCaps(caps: DbCapabilities): DbCapabilities {
  const bytes = caps.maxStreamedResultBytes ?? 0;
  if (bytes <= MAX_SCALAR_BYTES) {
    return caps;
  }
  return { ...caps, maxStreamedResultBytes: MAX_SCALAR_BYTES };
}

/**
 * Wraps an isolate adapter's streamed frames as the invoke reply's `inline`
 * member, or as a `limit_exceeded` error when they do not fit one scalar.
 *
 * Only a real `Uint8Array` is encoded: a look-alike object could report a
 * small `byteLength` and a huge `length`, and the encoder would size its
 * buffer from the latter.
 *
 * @param frames - Every `ExecuteStreamFrame` the adapter produced, concatenated.
 * @returns The `ExecuteStreamReply` the launcher relays to the host.
 */
function inlineStreamReply(frames: unknown): ExecuteStreamReply {
  if (!(frames instanceof Uint8Array)) {
    throw new InvokeError(500, "internal", "executeStreamed must return a Uint8Array of frames");
  }
  if (frames.byteLength > MAX_SCALAR_BYTES) {
    return {
      kind: "err",
      value: wireError(
        PluginError.limitExceeded({
          limit: "maxScalarBytes",
          cap: MAX_SCALAR_BYTES,
          used: frames.byteLength,
        }),
      ),
    };
  }
  return { kind: "inline", value: frames };
}

function wireError(err: unknown): WirePluginError {
  const e = err as { wireCode?: unknown; code?: unknown; message?: unknown } | null;
  const code =
    e && typeof e === "object"
      ? typeof e.wireCode === "string"
        ? e.wireCode
        : typeof e.code === "string"
          ? e.code
          : "internal"
      : "internal";
  const message = err instanceof Error ? err.message : typeof err === "string" ? err : String(err);
  return { code, message, details: wireDetails(err) };
}

/**
 * Reads `details` structurally, so a `PluginError` from another copy of the
 * SDK (a bundled dependency, another realm) keeps its details on the wire.
 *
 * @param err - Thrown value.
 * @returns `details` as JSON object text, or `""` when there is none.
 */
function wireDetails(err: unknown): string {
  if (err instanceof PluginError) return err.detailsText;
  const details = (err as { details?: unknown } | null)?.details;
  if (details === null || typeof details !== "object" || Array.isArray(details)) return "";
  try {
    return JSON.stringify(details) ?? "";
  } catch {
    return "";
  }
}

function resolveStub(entry: AnySpec, host: InvokeHost): Stub {
  if (entry.stub === "author") {
    const author = host.author();
    if (!author) throw new InvokeError(500, "unavailable", "PLUGIN binding missing");
    return author;
  }
  const stub = host.named(entry.stub);
  if (!stub) throw new InvokeError(404, "unsupported", `${entry.stub} entrypoint not exported`);
  return stub;
}

/**
 * Dispatch one `/invoke` call.
 *
 * @param iface - Cap'n interface name (`X-Bookclerk-Interface`).
 * @param method - Method name (`X-Bookclerk-Method`).
 * @param context - Decoded bridge context, or `undefined` for `PluginWorker.*`.
 * @param caps - Request capability descriptors (`X-Bookclerk-Caps`).
 * @param target - Isolate object id (`X-Bookclerk-Target`), or `null`.
 * @param body - `$Params` message bytes.
 * @param host - Adapter hooks.
 * @returns `$Results` bytes plus reply capability descriptors.
 * @throws {InvokeError} On transport-level failures (`400` / `404` / `500`).
 * @internal
 */
export async function dispatchInvoke(
  iface: string,
  method: string,
  context: BridgeContext | undefined,
  caps: CapDescriptor[],
  target: string | null,
  body: Uint8Array,
  host: InvokeHost,
): Promise<InvokeReply> {
  const table = Object.hasOwn(METHODS, iface) ? METHODS[iface] : undefined;
  const entry = table && Object.hasOwn(table, method) ? table[method] : undefined;
  if (!entry) {
    throw new InvokeError(400, "invalid_params", `unknown /invoke method ${iface}.${method}`);
  }
  const stop = new AbortController();
  let params: unknown;
  try {
    params = W.decodeMessage(entry.params, body, requestCapTable(caps, host, stop.signal));
  } catch (err) {
    stop.abort();
    if (err instanceof InvokeError) throw err;
    throw new InvokeError(
      400,
      "invalid_params",
      `malformed ${iface}.${method} params: ${wireError(err).message}`,
    );
  }
  let reply: unknown;
  try {
    const stub = resolveStub(entry, host);
    const cx: CallContext = { host, ctx: context, target, stop };
    try {
      reply = entry.ok(await entry.call(stub, params, cx));
    } catch (err) {
      if (err instanceof InvokeError) throw err;
      reply = { kind: "err", value: wireError(err) };
    }
  } finally {
    stop.abort();
  }
  const replyCaps: CapDescriptor[] = [];
  const bytes = W.encodeMessage(entry.results, { result: reply }, replyCapTable(replyCaps));
  return { body: bytes, caps: replyCaps };
}
