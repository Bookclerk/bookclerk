/// <reference path="./cloudflare-workers.d.ts" />
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
 * methods their `capabilities.methods` declare.
 *
 * Required: {@link handshake}.
 *
 * Defaults: {@link shutdown}, {@link health}, {@link diagnose}.
 *
 * Optional overrides (omit or leave unimplemented until needed):
 * `onEvent`, `cliDescribe`, `cliInvoke`, `start`, `pollEvents`, kind-specific
 * methods (`scan`, `fetchTitle`, storage/db ops, …).
 */
export abstract class BookclerkPlugin extends WorkerEntrypoint<BookclerkEnv> {
  /** Bookclerk host bindings (`HOST`, `CONFIG`, `PLUGIN_KV`, …). */
  declare readonly env: BookclerkEnv;

  /** Required by workerd when the entrypoint is not HTTP-facing. */
  async fetch(): Promise<Response> {
    return new Response(null, { status: 404 });
  }

  /** Identity, capabilities, CLI schema, brand. */
  abstract handshake(params: HandshakeParams): Promise<HandshakeResult>;

  /** Graceful teardown. */
  async shutdown(): Promise<void> {}

  /** Liveness probe. */
  async health(): Promise<HealthResult> {
    return { ok: true };
  }

  /** Human-readable diagnostic lines. */
  async diagnose(): Promise<DiagnoseResult> {
    return { lines: [] };
  }

  /**
   * Host → plugin event delivery (`onEvent`).
   * Override when advertising the `onEvent` capability.
   */
  async onEvent(_event: HostToPluginEvent): Promise<void> {
    throw unsupported("onEvent");
  }

  /** Declared CLI surface (mirrors handshake `cli` when present). */
  async cliDescribe(): Promise<CliSchema> {
    return { commands: [] };
  }

  /**
   * Run a plugin CLI command (`cliInvoke`).
   * Override when advertising the `cli` capability.
   */
  async cliInvoke(_params: CliInvokeParams): Promise<CliInvokeResult> {
    throw unsupported("cliInvoke");
  }

  async start(_params?: unknown): Promise<void> {
    throw unsupported("start");
  }

  async pollEvents(): Promise<unknown> {
    throw unsupported("pollEvents");
  }

  async scanLibrary(_params: unknown): Promise<void> {
    throw unsupported("scanLibrary");
  }

  async syncListening(): Promise<unknown> {
    throw unsupported("syncListening");
  }

  async authenticateUser(_params: unknown): Promise<unknown> {
    throw unsupported("authenticateUser");
  }

  async login(_params: unknown): Promise<unknown> {
    throw unsupported("login");
  }

  async loginStart(_params: unknown): Promise<unknown> {
    throw unsupported("loginStart");
  }

  async loginComplete(_params: unknown): Promise<unknown> {
    throw unsupported("loginComplete");
  }

  async credentialsUpdate(_params: unknown): Promise<void> {
    throw unsupported("credentialsUpdate");
  }

  async scan(_params: unknown): Promise<unknown> {
    throw unsupported("scan");
  }

  async fetchTitle(_params: unknown): Promise<unknown> {
    throw unsupported("fetchTitle");
  }

  async searchCatalog(_params: unknown): Promise<unknown> {
    throw unsupported("searchCatalog");
  }

  async expandCandidates(_params: unknown): Promise<unknown> {
    throw unsupported("expandCandidates");
  }

  async purchaseHint(_params: unknown): Promise<unknown> {
    throw unsupported("purchaseHint");
  }

  async listDeals(_params: unknown): Promise<unknown> {
    throw unsupported("listDeals");
  }

  async listAccounts(_params: unknown): Promise<unknown> {
    throw unsupported("listAccounts");
  }

  async catalogDetail(_params: unknown): Promise<unknown> {
    throw unsupported("catalogDetail");
  }

  async put(_params: unknown): Promise<void> {
    throw unsupported("put");
  }

  async putFile(_params: unknown): Promise<void> {
    throw unsupported("putFile");
  }

  async get(_params: unknown): Promise<unknown> {
    throw unsupported("get");
  }

  async exists(_params: unknown): Promise<boolean> {
    throw unsupported("exists");
  }

  async list(_params: unknown): Promise<unknown> {
    throw unsupported("list");
  }

  async probe(_params: unknown): Promise<unknown> {
    throw unsupported("probe");
  }

  async copy(_params: unknown): Promise<void> {
    throw unsupported("copy");
  }

  async delete(_params: unknown): Promise<void> {
    throw unsupported("delete");
  }

  async touchFile(_params: unknown): Promise<void> {
    throw unsupported("touchFile");
  }

  async dbConnect(_params: unknown): Promise<unknown> {
    throw unsupported("dbConnect");
  }

  async dbPing(): Promise<void> {
    throw unsupported("dbPing");
  }

  async dbQuery(_params: unknown): Promise<unknown> {
    throw unsupported("dbQuery");
  }

  async dbExecute(_params: unknown): Promise<unknown> {
    throw unsupported("dbExecute");
  }
}

function unsupported(method: string): Error {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported" as const,
  });
}
