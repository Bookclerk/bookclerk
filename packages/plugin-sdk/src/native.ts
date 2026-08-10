/**
 * Native (stdio) guest — same branded {@link BookclerkPlugin} contract as workerd.
 *
 * Dual-stack:
 * - Workerd: `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"`
 * - Native:  `import { BookclerkPlugin, BookclerkPluginGuest } from "@bookclerk/plugin-sdk/native"`
 *
 * {@link BookclerkPluginGuest} is the native stdin/stdout Workers RPC runner
 * (workerd hosts the class via WorkerEntrypoint instead). Authors subclass
 * {@link BookclerkPlugin}; plain objects with the same methods also work.
 */

import * as readline from "node:readline";
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
 * Prefer extending {@link BookclerkPlugin}.
 */
export type BookclerkPluginLike = {
  handshake(
    params: HandshakeParams,
  ): Promise<HandshakeResult> | HandshakeResult;
  shutdown?(): Promise<void> | void;
  health?(): Promise<HealthResult> | HealthResult;
  diagnose?(): Promise<DiagnoseResult> | DiagnoseResult;
  onEvent?(event: HostToPluginEvent): Promise<void> | void;
  cliDescribe?(): Promise<CliSchema> | CliSchema;
  cliInvoke?(params: CliInvokeParams): Promise<CliInvokeResult> | CliInvokeResult;
  start?(params?: unknown): Promise<void> | void;
  pollEvents?(): Promise<unknown> | unknown;
  scanLibrary?(params: unknown): Promise<void> | void;
  syncListening?(): Promise<unknown> | unknown;
  authenticateUser?(params: unknown): Promise<unknown> | unknown;
  login?(params: unknown): Promise<unknown> | unknown;
  loginStart?(params: unknown): Promise<unknown> | unknown;
  loginComplete?(params: unknown): Promise<unknown> | unknown;
  credentialsUpdate?(params: unknown): Promise<void> | void;
  scan?(params: unknown): Promise<unknown> | unknown;
  fetchTitle?(params: unknown): Promise<unknown> | unknown;
  searchCatalog?(params: unknown): Promise<unknown> | unknown;
  expandCandidates?(params: unknown): Promise<unknown> | unknown;
  purchaseHint?(params: unknown): Promise<unknown> | unknown;
  listDeals?(params: unknown): Promise<unknown> | unknown;
  listAccounts?(params: unknown): Promise<unknown> | unknown;
  catalogDetail?(params: unknown): Promise<unknown> | unknown;
  put?(params: unknown): Promise<void> | void;
  putFile?(params: unknown): Promise<void> | void;
  get?(params: unknown): Promise<unknown> | unknown;
  exists?(params: unknown): Promise<boolean> | boolean;
  list?(params: unknown): Promise<unknown> | unknown;
  probe?(params: unknown): Promise<unknown> | unknown;
  copy?(params: unknown): Promise<void> | void;
  delete?(params: unknown): Promise<void> | void;
  touchFile?(params: unknown): Promise<void> | void;
  dbConnect?(params: unknown): Promise<unknown> | unknown;
  dbPing?(): Promise<void> | void;
  dbQuery?(params: unknown): Promise<unknown> | unknown;
  dbExecute?(params: unknown): Promise<unknown> | unknown;
  callRaw?(method: string, params: unknown): Promise<unknown> | unknown;
};

/**
 * Branded native guest base — method surface matches workerd BookclerkPlugin.
 */
export abstract class BookclerkPlugin implements BookclerkPluginLike {
  abstract handshake(
    params: HandshakeParams,
  ): Promise<HandshakeResult> | HandshakeResult;

  async shutdown(): Promise<void> {}

  async health(): Promise<HealthResult> {
    return { ok: true };
  }

  async diagnose(): Promise<DiagnoseResult> {
    return { lines: [] };
  }

  async onEvent(_event: HostToPluginEvent): Promise<void> {
    throw unsupported("onEvent");
  }

  async cliDescribe(): Promise<CliSchema> {
    return { commands: [] };
  }

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

  async callRaw(_method: string, _params: unknown): Promise<unknown> {
    throw unsupported("callRaw");
  }
}

type RpcRequest = { id?: unknown; method?: string; params?: unknown };

/**
 * Native guest runner — hosts a {@link BookclerkPlugin} on stdin/stdout.
 * Analogous to Rust `BookclerkPluginGuest` / low-level `PluginGuest`.
 */
export class BookclerkPluginGuest {
  /** Run the Workers RPC loop until stdin closes or after a successful `shutdown`. */
  static async serve(plugin: BookclerkPluginLike): Promise<void> {
    const rl = readline.createInterface({ input: process.stdin, terminal: false });
    for await (const line of rl) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let req: RpcRequest;
      try {
        req = JSON.parse(trimmed) as RpcRequest;
      } catch (err) {
        writeError(null, `invalid JSON: ${err instanceof Error ? err.message : String(err)}`);
        continue;
      }
      const id = req.id ?? null;
      const method = req.method ?? "";
      try {
        const result = await dispatch(plugin, method, req.params);
        writeResult(id, result);
        if (method === "shutdown") {
          rl.close();
          return;
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        const code =
          err && typeof err === "object" && "code" in err
            ? String((err as { code: unknown }).code)
            : "internal";
        writeError(id, message, code);
      }
    }
  }
}

async function dispatch(
  plugin: BookclerkPluginLike,
  method: string,
  params: unknown,
): Promise<unknown> {
  switch (method) {
    case "handshake":
      return plugin.handshake((params ?? {}) as HandshakeParams);
    case "shutdown":
      await plugin.shutdown?.();
      return null;
    case "health":
      return (await plugin.health?.()) ?? { ok: true };
    case "diagnose":
      return (await plugin.diagnose?.()) ?? { lines: [] };
    case "onEvent":
      if (!plugin.onEvent) throw unsupported("onEvent");
      await plugin.onEvent(params as HostToPluginEvent);
      return { ok: true };
    case "cliDescribe":
      return (await plugin.cliDescribe?.()) ?? { commands: [] };
    case "cliInvoke":
      if (!plugin.cliInvoke) throw unsupported("cliInvoke");
      return plugin.cliInvoke(params as CliInvokeParams);
    case "start":
      if (!plugin.start) throw unsupported("start");
      await plugin.start(params);
      return null;
    case "pollEvents":
      if (!plugin.pollEvents) throw unsupported("pollEvents");
      return plugin.pollEvents();
    case "scanLibrary":
      if (!plugin.scanLibrary) throw unsupported("scanLibrary");
      await plugin.scanLibrary(params);
      return null;
    case "syncListening":
      if (!plugin.syncListening) throw unsupported("syncListening");
      return plugin.syncListening();
    case "authenticateUser":
      if (!plugin.authenticateUser) throw unsupported("authenticateUser");
      return plugin.authenticateUser(params);
    case "login":
      if (!plugin.login) throw unsupported("login");
      return plugin.login(params);
    case "loginStart":
      if (!plugin.loginStart) throw unsupported("loginStart");
      return plugin.loginStart(params);
    case "loginComplete":
      if (!plugin.loginComplete) throw unsupported("loginComplete");
      return plugin.loginComplete(params);
    case "credentialsUpdate":
      if (!plugin.credentialsUpdate) throw unsupported("credentialsUpdate");
      await plugin.credentialsUpdate(params);
      return null;
    case "scan":
      if (!plugin.scan) throw unsupported("scan");
      return plugin.scan(params);
    case "fetchTitle":
      if (!plugin.fetchTitle) throw unsupported("fetchTitle");
      return plugin.fetchTitle(params);
    case "searchCatalog":
      if (!plugin.searchCatalog) throw unsupported("searchCatalog");
      return plugin.searchCatalog(params);
    case "expandCandidates":
      if (!plugin.expandCandidates) throw unsupported("expandCandidates");
      return plugin.expandCandidates(params);
    case "purchaseHint":
      if (!plugin.purchaseHint) throw unsupported("purchaseHint");
      return plugin.purchaseHint(params);
    case "listDeals":
      if (!plugin.listDeals) throw unsupported("listDeals");
      return plugin.listDeals(params);
    case "listAccounts":
      if (!plugin.listAccounts) throw unsupported("listAccounts");
      return plugin.listAccounts(params);
    case "catalogDetail":
      if (!plugin.catalogDetail) throw unsupported("catalogDetail");
      return plugin.catalogDetail(params);
    case "put":
      if (!plugin.put) throw unsupported("put");
      await plugin.put(params);
      return null;
    case "putFile":
      if (!plugin.putFile) throw unsupported("putFile");
      await plugin.putFile(params);
      return null;
    case "get":
      if (!plugin.get) throw unsupported("get");
      return plugin.get(params);
    case "exists":
      if (!plugin.exists) throw unsupported("exists");
      return plugin.exists(params);
    case "list":
      if (!plugin.list) throw unsupported("list");
      return plugin.list(params);
    case "probe":
      if (!plugin.probe) throw unsupported("probe");
      return plugin.probe(params);
    case "copy":
      if (!plugin.copy) throw unsupported("copy");
      await plugin.copy(params);
      return null;
    case "delete":
      if (!plugin.delete) throw unsupported("delete");
      await plugin.delete(params);
      return null;
    case "touchFile":
      if (!plugin.touchFile) throw unsupported("touchFile");
      await plugin.touchFile(params);
      return null;
    case "dbConnect":
      if (!plugin.dbConnect) throw unsupported("dbConnect");
      return plugin.dbConnect(params);
    case "dbPing":
      if (!plugin.dbPing) throw unsupported("dbPing");
      await plugin.dbPing();
      return null;
    case "dbQuery":
      if (!plugin.dbQuery) throw unsupported("dbQuery");
      return plugin.dbQuery(params);
    case "dbExecute":
      if (!plugin.dbExecute) throw unsupported("dbExecute");
      return plugin.dbExecute(params);
    default:
      if (plugin.callRaw) return plugin.callRaw(method, params);
      throw unsupported(method);
  }
}

function unsupported(method: string): Error {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported" as const,
  });
}

function writeResult(id: unknown, result: unknown): void {
  process.stdout.write(JSON.stringify({ id, result }) + "\n");
}

function writeError(id: unknown, message: string, code = "internal"): void {
  process.stdout.write(JSON.stringify({ id, error: { code, message } }) + "\n");
}
