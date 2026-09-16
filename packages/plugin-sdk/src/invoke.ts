/**
 * `POST /invoke` dispatch for the trusted adapter isolate: decodes one
 * `<Interface>.<method>$Params` Cap'n Proto message, calls the author class
 * through its `bookclerk*` dispatch surface, and encodes the
 * `<Interface>.<method>$Results` reply. See `docs/workerd-bridge.md`.
 *
 * Author failures ride inside the reply union (`result.err`); only transport
 * failures (unknown method, malformed bytes, missing binding) surface as
 * {@link InvokeError} with the HTTP status the bridge answers with.
 *
 * @internal
 * @module
 */

import type { CapTable } from "./db-capnp.js";
import { PluginError } from "./errors.js";
import type {
  AdapterDatabaseSession as WireAdapterDatabaseSession,
  AdapterExecuteRequest,
  CatalogHit,
  DbBootstrap,
  DbCapabilities,
  EventResult,
  ExecuteReply,
  ExternalUser,
  IdentityHighWater,
  JobController as WireJobController,
  JobOutcome,
  ListeningProgress,
  ObjectMetadata as WireObjectMetadata,
  OidcClientTemplate,
  PluginError as WirePluginError,
  PluginMigration as WirePluginMigration,
  PurchaseHint,
  SourceAccount,
} from "./generated.js";
import * as W from "./generated-wire.js";
import type {
  AuthorStub,
  BridgeContext,
  EntrypointName,
  EventOutcome,
  GrantedContext,
  GrantedJobCapabilities,
  JobOutcomeRecord,
  ListOptions,
  NamedStub,
  ObjectMetadata,
  PluginMigration,
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

const okHits = (value: unknown) => okValue({ hits: asList<CatalogHit>(value) });
const okLines = (value: unknown) => okValue({ lines: asList<unknown>(value).map((l) => String(l)) });
const okHealth = (value: unknown) => {
  const v = (value ?? {}) as { ok?: unknown; detail?: unknown };
  return okValue({ ok: Boolean(v.ok), detail: String(v.detail ?? "") });
};

function eventResult(outcome: EventOutcome): EventResult {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack", value: { dummy: undefined } };
    case "retry":
      return {
        kind: "retry",
        value: { retryAtUnixMs: Number(outcome.retryAtUnixMs) || 0, reason: String(outcome.reason ?? "") },
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
      throw PluginError.fromWire("internal", `unknown event outcome ${String((outcome as { kind: unknown }).kind)}`);
  }
}

function jobOutcome(record: JobOutcomeRecord): JobOutcome {
  switch (record.kind) {
    case "completed":
      return {
        kind: "completed",
        value: { message: String(record.message ?? ""), bytesCopied: Number(record.bytesCopied) || 0 },
      };
    case "retryable":
      return {
        kind: "retryable",
        value: { message: String(record.message ?? ""), retryAfterUnixMs: Number(record.retryAfterUnixMs) || 0 },
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
      throw PluginError.fromWire("internal", `unknown job outcome ${String((record as { kind: unknown }).kind)}`);
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
      if (o.kind === "schema" || o.kind === "data") return { kind: o.kind, value: String(o.value ?? "") };
      throw PluginError.fromWire("invalid_params", `migration \`${String(migration?.id)}\` has an op without schema/data`);
    }),
  };
}

function listOptions(options: Partial<W.DestinationListParams["options"]> | undefined): ListOptions {
  return {
    prefix: options?.prefix || undefined,
    cursor: options?.cursor || undefined,
    limit: options?.limit || undefined,
  };
}

function sessionCall(method: string): AnySpec["call"] {
  return (s, p, cx) => {
    if (!cx.target) {
      throw new InvokeError(400, "invalid_params", `AdapterDatabaseSession.${method} requires X-Bookclerk-Target`);
    }
    const params = p as Record<string, unknown>;
    const args =
      method === "execute"
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
    databaseMigrations: spec<W.PluginWorkerDatabaseMigrationsParams, W.PluginWorkerDatabaseMigrationsResults["result"]>({
      params: W.PluginWorkerDatabaseMigrationsParamsCodec,
      results: W.PluginWorkerDatabaseMigrationsResultsCodec,
      stub: "author",
      call: (s, p) => (s as AuthorStub).bookclerkDatabaseMigrations(String(p.binding ?? "")),
      ok: (value) => okValue({ migrations: asList<PluginMigration>(value).map(wireMigration) }),
    }),
  },
  ContentSource: {
    login: withParams("storefront", "login", W.ContentSourceLoginParamsCodec, W.ContentSourceLoginResultsCodec, okPass),
    scan: withParams("storefront", "scan", W.ContentSourceScanParamsCodec, W.ContentSourceScanResultsCodec, okPass),
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
      (value) => okValue({ found: value != null, hint: (value ?? undefined) as PurchaseHint }),
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
  },
  Destination: {
    head: spec<W.DestinationHeadParams, W.DestinationHeadResults["result"]>({
      params: W.DestinationHeadParamsCodec,
      results: W.DestinationHeadResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "head", [String(p.key ?? "")]),
      ok: (value) => {
        const meta = (value ?? null) as ObjectMetadata | null;
        return okValue({ found: meta != null, meta: (meta ?? undefined) as unknown as WireObjectMetadata });
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
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "copy", [String(p.from ?? ""), String(p.to ?? "")]),
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
        named(s, cx.host.bindContext(cx.ctx), "commit", [String(p.key ?? ""), String(p.commitToken ?? "")]),
      ok: okPass,
    }),
    abortStage: spec<W.DestinationAbortStageParams, W.DestinationAbortStageResults["result"]>({
      params: W.DestinationAbortStageParamsCodec,
      results: W.DestinationAbortStageResultsCodec,
      stub: "storage",
      call: (s, p, cx) =>
        named(s, cx.host.bindContext(cx.ctx), "abortStage", [String(p.key ?? ""), String(p.commitToken ?? "")]),
      ok: okEmpty,
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
    start: withoutParams("remoteLibrary", "start", W.RemoteLibraryStartParamsCodec, W.RemoteLibraryStartResultsCodec, okEmpty),
    stop: withoutParams("remoteLibrary", "stop", W.RemoteLibraryStopParamsCodec, W.RemoteLibraryStopResultsCodec, okEmpty),
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
        return (s as AuthorStub).bookclerkJob(cx.host.bindContext(cx.ctx), controller.invocation ?? {}, granted);
      },
      ok: (value) => okValue(jobOutcome(value as JobOutcomeRecord)),
    }),
  },
  PluginCli: {
    describe: withoutParams("cli", "describe", W.PluginCliDescribeParamsCodec, W.PluginCliDescribeResultsCodec, okPass),
    invoke: withParams("cli", "invoke", W.PluginCliInvokeParamsCodec, W.PluginCliInvokeResultsCodec, okPass),
  },
  Oidc: {
    clients: withoutParams("oidc", "clients", W.OidcClientsParamsCodec, W.OidcClientsResultsCodec, (value) =>
      okValue({ clients: asList<OidcClientTemplate>(value) }),
    ),
    authenticateUser: withParams(
      "oidc",
      "authenticateUser",
      W.OidcAuthenticateUserParamsCodec,
      W.OidcAuthenticateUserResultsCodec,
      okPass,
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
  },
  AdapterDatabaseSession: {
    capabilities: session(
      "capabilities",
      W.AdapterDatabaseSessionCapabilitiesParamsCodec,
      W.AdapterDatabaseSessionCapabilitiesResultsCodec,
      (value) => okValue(value as DbCapabilities),
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
  },
};

const CAP_KINDS: ReadonlySet<string> = new Set(["source", "destination", "progress", "cancellation"]);

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
        throw new InvokeError(400, "invalid_params", `unsupported request capability kind ${descriptor.kind}`);
      }
      const value = host.importCap(descriptor, stop);
      if (value === undefined) {
        throw new InvokeError(400, "invalid_params", `unsupported request capability kind ${descriptor.kind}`);
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
  return { code, message };
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
    throw new InvokeError(400, "invalid_params", `malformed ${iface}.${method} params: ${wireError(err).message}`);
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
