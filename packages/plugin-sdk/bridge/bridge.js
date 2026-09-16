/**
 * Bookclerk bridge worker — HTTP ↔ Workers RPC service binding.
 *
 * `env.PLUGIN` is the trusted adapter isolate; this worker only turns
 * loopback HTTP from `bookclerk-workerd` into one adapter call per request
 * and retains nothing across requests. Three route families
 * (see `docs/workerd-bridge.md`):
 *
 * - Control plane, JSON: `GET /health`, `POST /describe` (optional
 *   `{ native }` body merged against `PLUGIN_DESCRIBE`), `POST /open`
 *   (`{ context, entrypoints }` → the authorized subset), `POST /shutdown`.
 *   Shared with native-behind-workerd, where only policy passes through here.
 * - Data plane, Cap'n Proto bytes: `POST /invoke` — every entrypoint method
 *   and `PluginWorker.databaseMigrations`. Interface, method, bridge-JSON
 *   context, capability descriptors, and target object id ride in
 *   `X-Bookclerk-*` headers; the body is the `$Params` struct and the `200`
 *   reply body the `$Results` struct (`X-Bookclerk-Caps` lists exported
 *   capabilities). ABI failures stay inside the reply union; non-200 answers
 *   are transport failures (`{ "error": { "code", "message" } }`).
 * - Streams, HTTP bodies: `GET /destination/get`, `PUT /destination/put`,
 *   `GET /source/open` — object bytes never enter a Cap'n message.
 *
 * Control-plane envelopes are the camelCase JSON projection of the typed ABI
 * structs with `Data` fields as base64 text; `fromBridgeJson` / `toBridgeJson`
 * convert them to and from Workers RPC values. Every request requires
 * `Authorization: Bearer` matching the per-isolate `BRIDGE_TOKEN` binding.
 */

function timingSafeEqual(a, b) {
  if (typeof a !== "string" || typeof b !== "string") return false;
  let out = a.length === b.length ? 0 : 1;
  for (let i = 0; i < b.length; i++) {
    const ac = i < a.length ? a.charCodeAt(i) : 0;
    out |= ac ^ b.charCodeAt(i);
  }
  return out === 0;
}

function authorize(request, env) {
  const expected = env.BRIDGE_TOKEN;
  if (typeof expected !== "string" || !expected) {
    return false;
  }
  const header = request.headers.get("Authorization") || "";
  const prefix = "Bearer ";
  if (!header.startsWith(prefix) && !header.startsWith("bearer ")) {
    return false;
  }
  const provided = header.slice(prefix.length).trim();
  return timingSafeEqual(provided, expected);
}

function errJson(id, code, message, status) {
  return Response.json(
    { id: id ?? null, error: { code, message } },
    { status: status ?? 200 },
  );
}

function catchErr(err) {
  const code =
    err && typeof err === "object" && typeof err.wireCode === "string"
      ? err.wireCode
      : err && typeof err === "object" && typeof err.code === "string"
        ? err.code
        : "internal";
  const message =
    err instanceof Error
      ? err.message
      : typeof err === "string"
        ? err
        : String(err);
  return { code, message };
}

function metaHeaders(meta) {
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

/**
 * Adapter method lookup. `env.PLUGIN` is a service-binding stub, so the
 * method is invoked through the stub rather than bound (`bind` is itself
 * an RPC call on a stub).
 */
function adapterMethod(plugin, name) {
  if (typeof plugin[name] !== "function") {
    throw Object.assign(new Error(`adapter missing ${name}`), { code: "internal" });
  }
  return (...args) => plugin[name](...args);
}

/** `storage` stream call through the adapter (`invokeDestination`: `get` / `put`). */
function invokeDest(plugin, op, ctx, args, body) {
  return adapterMethod(plugin, "invokeDestination")(op, ctx, args, body);
}

/** `storage.get` through the adapter for the `/source/open` route. */
function invokeSourceOpen(plugin, ctx, key) {
  return adapterMethod(plugin, "invokeSourceOpen")(ctx, key);
}

/** Cap'n `Data` field names; bridge JSON carries them as base64 text. */
const BYTES_FIELDS = new Set(["payload", "sha256", "credentials"]);

function base64ToBytes(text) {
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}

/**
 * Bridge JSON → Workers RPC value: base64 `Data` fields become `Uint8Array`.
 * Typed struct shapes are otherwise identical (camelCase, `$optional` absent
 * as `undefined`), so no per-struct codec is needed on this transport.
 */
function fromBridgeJson(value) {
  if (Array.isArray(value)) return value.map(fromBridgeJson);
  if (value === null || typeof value !== "object") return value;
  const out = {};
  for (const [key, inner] of Object.entries(value)) {
    if (BYTES_FIELDS.has(key) && typeof inner === "string") {
      out[key] = base64ToBytes(inner);
    } else if (BYTES_FIELDS.has(key) && Array.isArray(inner) && inner.every((n) => typeof n === "number")) {
      out[key] = Uint8Array.from(inner);
    } else {
      out[key] = fromBridgeJson(inner);
    }
  }
  return out;
}

/** Workers RPC value → bridge JSON: `Uint8Array` / `ArrayBuffer` become base64. */
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

function bridgeJson(value) {
  return Response.json(toBridgeJson(value ?? {}));
}

/**
 * Bridge-JSON context (`BridgeContext`: `invocation`, `config`, `secrets`,
 * `eventsToken`) from the `x-bookclerk-context` header or the `context`
 * body field, decoded to Workers RPC values.
 */
function contextFrom(request, body) {
  const header = request.headers.get("x-bookclerk-context");
  if (header) {
    try {
      return fromBridgeJson(JSON.parse(header));
    } catch {
      return {};
    }
  }
  if (body && typeof body === "object" && body.context && typeof body.context === "object") {
    return fromBridgeJson(body.context);
  }
  return {};
}

/** Header value, or `null` when absent / empty. */
function headerOrNull(request, name) {
  const value = request.headers.get(name);
  return value === null || value === "" ? null : value;
}

/**
 * `POST /invoke`: one ABI method call as Cap'n Proto bytes. The adapter
 * decodes the header strings itself (`invoke(iface, method, contextJson,
 * capsJson, target, body)`) and answers either `{ body, caps }` or a plain
 * `{ status, error }` transport failure — a value rather than a thrown error,
 * because Workers RPC drops own properties such as `status` from exceptions.
 */
async function handleInvoke(request, plugin) {
  let reply;
  try {
    const iface = request.headers.get("x-bookclerk-interface") || "";
    const method = request.headers.get("x-bookclerk-method") || "";
    const context = headerOrNull(request, "x-bookclerk-context");
    const caps = headerOrNull(request, "x-bookclerk-caps");
    const target = headerOrNull(request, "x-bookclerk-target");
    const body = new Uint8Array(await request.arrayBuffer());
    reply = await adapterMethod(plugin, "invoke")(iface, method, context, caps, target, body);
  } catch (err) {
    const { code, message } = catchErr(err);
    const status = Number(err && typeof err === "object" ? err.status : 0) || 500;
    return errJson(null, code, message, status);
  }
  if (!reply || typeof reply !== "object") {
    return errJson(null, "internal", "adapter invoke returned no reply", 500);
  }
  if (reply.error) {
    const status = Number(reply.status) || 500;
    const code = typeof reply.error.code === "string" ? reply.error.code : "internal";
    const message = typeof reply.error.message === "string" ? reply.error.message : "";
    return errJson(null, code, message, status);
  }
  const bytes =
    reply.body instanceof Uint8Array
      ? reply.body
      : reply.body instanceof ArrayBuffer
        ? new Uint8Array(reply.body)
        : null;
  if (!bytes) {
    return errJson(null, "internal", "adapter invoke returned no message bytes", 500);
  }
  return new Response(bytes, {
    status: 200,
    headers: {
      "content-type": "application/x-capnp",
      "x-bookclerk-caps": JSON.stringify(Array.isArray(reply.caps) ? reply.caps : []),
    },
  });
}

async function handleRoute(request, env, url) {
  const plugin = env.PLUGIN;
  if (!plugin) {
    return errJson(null, "unavailable", "PLUGIN binding missing", 500);
  }

  if (request.method === "POST" && url.pathname === "/invoke") {
    return handleInvoke(request, plugin);
  }

  if (request.method === "POST" && url.pathname === "/describe") {
    try {
      const body = await request.json().catch(() => ({}));
      const native =
        body && typeof body === "object" && body.native && typeof body.native === "object"
          ? fromBridgeJson(body.native)
          : undefined;
      const result = native === undefined ? await plugin.describe() : await plugin.describe(native);
      return bridgeJson(result);
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/open") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      const requested = Array.isArray(body.entrypoints)
        ? body.entrypoints.filter((name) => typeof name === "string")
        : [];
      const allowed = await adapterMethod(plugin, "openInvocation")(ctx, requested);
      return Response.json({ entrypoints: Array.isArray(allowed) ? allowed : [] });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/shutdown") {
    try {
      if (typeof plugin.shutdown === "function") {
        await plugin.shutdown();
      }
      return Response.json({ ok: true });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "GET" && url.pathname === "/destination/get") {
    try {
      const ctx = contextFrom(request, null);
      const key = url.searchParams.get("key") || "";
      const offset = url.searchParams.get("offset");
      const length = url.searchParams.get("length");
      const options =
        offset != null
          ? {
              range: {
                offset: Number(offset),
                length: length != null ? Number(length) : undefined,
              },
            }
          : undefined;
      const result = await invokeDest(plugin, "get", ctx, { key, options });
      return new Response(result.body, { headers: metaHeaders(result.meta) });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "PUT" && url.pathname === "/destination/put") {
    try {
      const ctx = contextFrom(request, null);
      const key = url.searchParams.get("key") || "";
      const contentType = request.headers.get("content-type") || undefined;
      const lenHeader = request.headers.get("content-length");
      const commitToken = request.headers.get("x-bookclerk-commit-token") || undefined;
      const stageOnly = request.headers.get("x-bookclerk-stage-only") === "1";
      const options = {
        contentType,
        contentLength: lenHeader != null ? Number(lenHeader) : undefined,
        commitToken,
        stageOnly,
      };
      const result = await invokeDest(plugin, "put", ctx, { key, options }, request.body);
      return bridgeJson(result);
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "GET" && url.pathname === "/source/open") {
    try {
      const ctx = contextFrom(request, null);
      const key = url.searchParams.get("key") || "";
      const result = await invokeSourceOpen(plugin, ctx, key);
      return new Response(result.body, { headers: metaHeaders(result.meta) });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  return new Response("not found", { status: 404 });
}

export default {
  async fetch(request, env) {
    if (!authorize(request, env)) {
      return new Response("unauthorized", { status: 401 });
    }

    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return new Response("ok", { status: 200 });
    }
    return handleRoute(request, env, url);
  },
};
