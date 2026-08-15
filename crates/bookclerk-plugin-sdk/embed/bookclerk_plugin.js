/**
 * Workerd runtime for `@bookclerk/plugin-sdk` / `@bookclerk/plugin-sdk/workerd`.
 *
 * Authors import the package — never a relative embed path:
 *
 *   import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd";
 *   // or: import { BookclerkPlugin } from "@bookclerk/plugin-sdk";
 *
 *   import { wasmBookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"; // Rust/Wasm glue
 *
 * `bookclerk-workerd` injects this module into the isolate under those names.
 * Native guests use Rust `serve` / `PluginRoot` instead.
 */

import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";

function unsupported(method) {
  return Object.assign(new Error(`${method} not implemented`), {
    code: "unsupported",
  });
}

export class BookclerkPluginLegacy extends WorkerEntrypoint {
  /** Required by workerd when the entrypoint is not HTTP-facing. */
  async fetch() {
    return new Response(null, { status: 404 });
  }

  /** Identity, capabilities, CLI schema, brand — required. */
  async handshake(_params) {
    throw unsupported("handshake");
  }

  async shutdown() {}

  async health() {
    return { ok: true };
  }

  async diagnose() {
    return { lines: [] };
  }

  async onEvent(_event) {
    throw unsupported("onEvent");
  }

  async cliDescribe() {
    return { commands: [] };
  }

  async cliInvoke(_params) {
    throw unsupported("cliInvoke");
  }
}

/**
 * BookclerkPlugin subclass that forwards Workers RPC methods to a Wasm
 * `dispatch(method, paramsJson) -> resultJson` export (wasm-bindgen).
 *
 * @param {(method: string, paramsJson: string) => string} dispatch
 * @returns {typeof BookclerkPluginLegacy}
 */
export function wasmBookclerkPlugin(dispatch) {
  return class WasmBookclerkPlugin extends BookclerkPluginLegacy {
    #call(method, params) {
      const paramsJson =
        params === undefined || params === null ? "{}" : JSON.stringify(params);
      const out = dispatch(method, paramsJson);
      return out === "null" ? null : JSON.parse(out);
    }

    async handshake(params) {
      return this.#call("handshake", params);
    }

    async shutdown() {
      this.#call("shutdown", {});
    }

    async health() {
      return this.#call("health", {});
    }

    async diagnose() {
      return this.#call("diagnose", {});
    }

    async onEvent(event) {
      this.#call("onEvent", event);
    }

    async cliDescribe() {
      return this.#call("cliDescribe", {});
    }

    async cliInvoke(params) {
      return this.#call("cliInvoke", params);
    }
  };
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

function v2Unsupported(method) {
  return PluginError.fromWire("unsupported", `${method} not implemented`);
}

/** Product ABI version 2 (`describe().apiVersion`). */
export const PRODUCT_API_VERSION = 2;
export const MAX_SCALAR_BYTES = 262144;
export const MAX_STREAM_WINDOW_BYTES = 1048576;
export const MAX_LIST_PAGE = 256;
export const FEATURE_SCALAR_LIMITS = "rpc.scalarLimits";
export const FEATURE_STREAMS = "rpc.streams";
export const FEATURE_STORAGE_COPY = "storage.copy";
export const ENVELOPE_VERSION = 1;
export const MAX_CHECKPOINT_BYTES = 65536;

/** Destination capability — subclass and override methods. Abort is stream cancel. */
export class Destination extends RpcTarget {
  async head(_key) {
    throw v2Unsupported("head");
  }
  async list(_options) {
    throw v2Unsupported("list");
  }
  async get(_key, _options) {
    throw v2Unsupported("get");
  }
  async put(_key, _body, _options) {
    throw v2Unsupported("put");
  }
  async copy(_from, _to) {
    throw v2Unsupported("copy");
  }
  async delete(_key) {
    throw v2Unsupported("delete");
  }
  async commit(_key, _commitToken) {
    throw v2Unsupported("commit");
  }
  async abortStage(_key, _commitToken) {
    throw v2Unsupported("abortStage");
  }
}

/** Source capability. */
export class Source extends RpcTarget {
  async open(_key) {
    throw v2Unsupported("open");
  }
}

/** Progress reports (never media). */
export class ProgressSink extends RpcTarget {
  async report(_percent, _message) {
    throw v2Unsupported("report");
  }
}

/** Job handler for one durable invocation. */
export class JobHandler extends RpcTarget {
  async handle(_invocation, _context) {
    throw v2Unsupported("handle");
  }
}

/** Storefront content source (not byte Source). */
export class ContentSource extends RpcTarget {
  async health() {
    return { ok: true };
  }
  async diagnose() {
    return { lines: [] };
  }
}

/** Integration role — health / diagnose / onEvent. */
export class Integration extends RpcTarget {
  async health() {
    return { ok: true };
  }
  async diagnose() {
    return { lines: [] };
  }
  async onEvent(_event) {
    throw v2Unsupported("onEvent");
  }
  async start() {}
  async stop() {}
}

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

function v2GrantedContext(env, grantToken, controller) {
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

function v2MetaHeaders(meta) {
  const headers = {
    "x-bookclerk-key": meta?.key || "",
    "x-bookclerk-size": String(meta?.size ?? 0),
  };
  if (meta?.contentType) {
    headers["x-bookclerk-content-type"] = meta.contentType;
    headers["content-type"] = meta.contentType;
  }
  if (meta?.etag) headers["x-bookclerk-etag"] = meta.etag;
  if (meta?.size != null && Number(meta.size) > 0) {
    headers["content-length"] = String(meta.size);
  }
  return headers;
}

function v2ErrResponse(err) {
  const code =
    err && typeof err === "object" && typeof err.wireCode === "string"
      ? err.wireCode
      : err && typeof err === "object" && typeof err.code === "string"
        ? err.code
        : "internal";
  const message = err instanceof Error ? err.message : String(err);
  return Response.json({ error: { code, message } }, { status: 200 });
}

/**
 * Product `apiVersion` 2 guest base — authors subclass this and export the
 * raw class. The trusted adapter creates, invokes, and disposes role stubs
 * in one request. Adapter-private GRANTED / BRIDGE_TOKEN / PLUGIN_BACKEND
 * never appear on author env.
 */
export class BookclerkPlugin extends WorkerEntrypoint {
  async fetch() {
    return new Response(null, { status: 404 });
  }
  async describe() {
    throw v2Unsupported("describe");
  }
  destination(_context) {
    throw v2Unsupported("destination");
  }
  source(_context) {
    throw v2Unsupported("source");
  }
  worker(_context) {
    throw v2Unsupported("worker");
  }
  contentSource(_ctx) {
    throw v2Unsupported("contentSource");
  }
  integration(_ctx) {
    throw v2Unsupported("integration");
  }
  database(_ctx) {
    throw v2Unsupported("database");
  }
  async cliDescribe() {
    return "{}";
  }
  async cliInvoke(_paramsJson) {
    throw v2Unsupported("cliInvoke");
  }
  async shutdown() {}
}

export { BookclerkPlugin as BookclerkPluginV2 };

async function disposeRpc(stub) {
  if (stub == null || typeof stub !== "object") return;
  try {
    if (typeof stub[Symbol.asyncDispose] === "function") {
      await stub[Symbol.asyncDispose]();
      return;
    }
    if (typeof stub[Symbol.dispose] === "function") {
      stub[Symbol.dispose]();
    }
  } catch {
    // disposal is best-effort
  }
}

export function wrapV2Plugin(Author) {
  return class WrappedAuthor extends Author {
    constructor(ctx, env) {
      const authorEnv = { ...env };
      delete authorEnv.GRANTED;
      delete authorEnv.BRIDGE_TOKEN;
      delete authorEnv.PLUGIN_BACKEND;
      delete authorEnv.PLUGIN;
      super(ctx, authorEnv);
    }
  };
}

export function wrapV2PluginFromBinding() {
  return createInvocationAdapter();
}

export function wrapV2PluginFromNative() {
  return createInvocationAdapter();
}

class HttpNativeDest extends Destination {
  constructor(fetcher, ctx) {
    super();
    this.fetcher = fetcher;
    this.ctx = ctx ?? {};
  }
  #headers(extra) {
    return {
      "content-type": "application/json",
      "x-bookclerk-context": JSON.stringify(this.ctx),
      ...(extra || {}),
    };
  }
  async #json(method, path, body) {
    const resp = await this.fetcher.fetch(`http://backend${path}`, {
      method,
      headers: this.#headers(),
      body: body == null ? undefined : JSON.stringify(body),
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `native broker HTTP ${resp.status}`);
    }
    return value;
  }
  async head(key) {
    const v = await this.#json("POST", "/v2/destination/head", { key, json: this.ctx.json });
    return v.found ? v.meta : null;
  }
  async list(options) {
    return this.#json("POST", "/v2/destination/list", { options, json: this.ctx.json });
  }
  async get(key, options) {
    let path = `/v2/destination/get?key=${encodeURIComponent(key)}`;
    if (options?.range) {
      path += `&offset=${options.range.offset}`;
      if (options.range.length != null) path += `&length=${options.range.length}`;
    }
    const resp = await this.fetcher.fetch(`http://backend${path}`, {
      headers: this.#headers(),
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
      body: resp.body,
    };
  }
  async put(key, body, options) {
    const headers = this.#headers();
    if (options?.contentType) headers["content-type"] = options.contentType;
    if (options?.contentLength != null) headers["content-length"] = String(options.contentLength);
    if (options?.commitToken) headers["x-bookclerk-commit-token"] = options.commitToken;
    if (options?.stageOnly) headers["x-bookclerk-stage-only"] = "1";
    const resp = await this.fetcher.fetch(
      `http://backend/v2/destination/put?key=${encodeURIComponent(key)}`,
      { method: "PUT", headers, body },
    );
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `native broker HTTP ${resp.status}`);
    }
    return value;
  }
  async copy(from, to) {
    return this.#json("POST", "/v2/destination/copy", { from, to, json: this.ctx.json });
  }
  async delete(key) {
    await this.#json("POST", "/v2/destination/delete", { key, json: this.ctx.json });
  }
  async commit(key, commitToken) {
    return this.#json("POST", "/v2/destination/commit", {
      key,
      commitToken,
      json: this.ctx.json,
    });
  }
  async abortStage(key, commitToken) {
    await this.#json("POST", "/v2/destination/abortStage", {
      key,
      commitToken,
      json: this.ctx.json,
    });
  }
}

class HttpNativeSource extends Source {
  constructor(fetcher, ctx) {
    super();
    this.fetcher = fetcher;
    this.ctx = ctx ?? {};
  }
  async open(key) {
    const resp = await this.fetcher.fetch(
      `http://backend/v2/source/open?key=${encodeURIComponent(key)}`,
      {
        headers: { "x-bookclerk-context": JSON.stringify(this.ctx) },
      },
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

class HttpNativeRoot {
  constructor(fetcher) {
    this.fetcher = fetcher;
  }
  async describe() {
    const resp = await this.fetcher.fetch("http://backend/v2/describe", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `native broker HTTP ${resp.status}`);
    }
    return value;
  }
  destination(ctx) {
    return new HttpNativeDest(this.fetcher, ctx);
  }
  source(ctx) {
    return new HttpNativeSource(this.fetcher, ctx);
  }
  worker() {
    throw PluginError.fromWire("unsupported", "native worker via broker not bound");
  }
  async shutdown() {}
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

function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint {
    plugin() {
      if (this.env.PLUGIN) return this.env.PLUGIN;
      if (this.env.PLUGIN_BACKEND) return nativeRoot(this.env);
      throw PluginError.fromWire("unavailable", "PLUGIN binding missing");
    }
    async fetch() {
      return new Response(null, { status: 404 });
    }
    async describe() {
      return this.plugin().describe();
    }
    destination(ctx) {
      return this.plugin().destination(ctx);
    }
    source(ctx) {
      return this.plugin().source(ctx);
    }
    worker(ctx) {
      return this.plugin().worker(ctx);
    }
    contentSource(ctx) {
      return this.plugin().contentSource(ctx);
    }
    integration(ctx) {
      return this.plugin().integration(ctx);
    }
    database(ctx) {
      return this.plugin().database(ctx);
    }
    async cliDescribe() {
      return this.plugin().cliDescribe();
    }
    async cliInvoke(paramsJson) {
      return this.plugin().cliInvoke(paramsJson);
    }
    async shutdown() {
      await this.plugin().shutdown();
    }
    async invokeDestination(op, ctx, args = {}, body) {
      const dest = await this.plugin().destination(ctx ?? {});
      try {
        switch (op) {
          case "head":
            return dest.head(String(args.key ?? ""));
          case "list":
            return dest.list(args.options ?? {});
          case "get":
            return dest.get(String(args.key ?? ""), args.options);
          case "put": {
            if (!body) {
              throw PluginError.fromWire("invalid_params", "put missing body stream");
            }
            const options = args.options;
            const bounded = exactLengthBody(body, options?.contentLength);
            return dest.put(String(args.key ?? ""), bounded, options);
          }
          case "copy":
            if (typeof dest.copy === "function") {
              return dest.copy(String(args.from ?? ""), String(args.to ?? ""));
            }
            throw PluginError.fromWire("unsupported", "copy not implemented");
          case "delete":
            await dest.delete(String(args.key ?? ""));
            return { ok: true };
          case "commit":
            return dest.commit(String(args.key ?? ""), String(args.commitToken ?? ""));
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
    async invokeSourceOpen(ctx, key) {
      const src = await this.plugin().source(ctx ?? {});
      try {
        return await src.open(key);
      } finally {
        await disposeRpc(src);
      }
    }
    async invokeHandle(ctx, invocation, grantToken) {
      const handler = await this.plugin().worker(ctx ?? {});
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

