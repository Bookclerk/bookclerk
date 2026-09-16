/**
 * Bookclerk bridge worker — HTTP ↔ Workers RPC service binding.
 *
 * Role routes (`/describe`, `/destination/*`, …): one invocation envelope per
 * HTTP request. The bridge creates the role capability, invokes the method,
 * and disposes the stub before completing. No dest-id table is retained
 * across requests.
 *
 * Envelopes are the camelCase JSON projection of the typed Cap'n Proto ABI
 * structs (`PluginDescribe`, `CliSchema`, `LoginParams`, …) with `Data`
 * fields as base64 text; `fromBridgeJson` / `toBridgeJson` convert them to
 * and from Workers RPC values. This transport is private to bookclerk-workerd.
 *
 * All role-route and `/health` requests require `Authorization: Bearer`
 * matching the per-isolate `BRIDGE_TOKEN` binding.
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

const MAX_LIST_PAGE = 256;
const MAX_PLUGIN_MIGRATION_OPS = 256;
const MAX_PLUGIN_MIGRATION_TOTAL_OPS = 2048;
const MAX_SCALAR_BYTES = 262144;
const MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144;

function utf8Bytes(value) {
  return new TextEncoder().encode(String(value ?? "")).byteLength;
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
    const err = new Error(
      `plugin migration count ${list.length} exceeds maxListPage (${MAX_LIST_PAGE})`,
    );
    err.code = "payload_too_large";
    err.wireCode = "payload_too_large";
    throw err;
  }
  let total = 0;
  let totalOps = 0;
  for (const migration of list) {
    const ops = Array.isArray(migration?.operations) ? migration.operations : [];
    if (ops.length > MAX_PLUGIN_MIGRATION_OPS) {
      const err = new Error(
        `plugin migration \`${migration?.id}\` has ${ops.length} operations; exceeds maxPluginMigrationOps (${MAX_PLUGIN_MIGRATION_OPS})`,
      );
      err.code = "payload_too_large";
      err.wireCode = "payload_too_large";
      throw err;
    }
    totalOps += ops.length;
    if (totalOps > MAX_PLUGIN_MIGRATION_TOTAL_OPS) {
      const err = new Error(
        `plugin migration registration has ${totalOps} operations; exceeds maxPluginMigrationTotalOps (${MAX_PLUGIN_MIGRATION_TOTAL_OPS})`,
      );
      err.code = "payload_too_large";
      err.wireCode = "payload_too_large";
      throw err;
    }
    total += utf8Bytes(migration?.id);
    if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
      const err = new Error(
        `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
      );
      err.code = "payload_too_large";
      err.wireCode = "payload_too_large";
      throw err;
    }
    for (const op of ops) {
      const n = utf8Bytes(migrationOpSql(op));
      if (n > MAX_SCALAR_BYTES) {
        const err = new Error(
          `plugin migration \`${migration?.id}\` SQL is ${n} bytes; exceeds maxScalarBytes (${MAX_SCALAR_BYTES})`,
        );
        err.code = "payload_too_large";
        err.wireCode = "payload_too_large";
        throw err;
      }
      total += n;
      if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
        const err = new Error(
          `plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`,
        );
        err.code = "payload_too_large";
        err.wireCode = "payload_too_large";
        throw err;
      }
    }
  }
  return list;
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

async function invokeDest(plugin, op, ctx, args, body) {
  if (typeof plugin.invokeDestination === "function") {
    return plugin.invokeDestination(op, ctx, args, body);
  }
  const dest = await plugin.destination(ctx);
  try {
    switch (op) {
      case "head":
        return dest.head(args.key || "");
      case "list":
        return dest.list(args.options ?? {});
      case "get":
        return dest.get(args.key || "", args.options);
      case "put":
        return dest.put(args.key || "", body, args.options);
      case "copy":
        if (typeof dest.copy !== "function") {
          throw Object.assign(new Error("copy not implemented"), { code: "unsupported" });
        }
        return dest.copy(args.from, args.to);
      case "delete":
        await dest.delete(args.key || "");
        return { ok: true };
      case "commit":
        return dest.commit(args.key || "", args.commitToken || "");
      case "abortStage":
        await dest.abortStage(args.key || "", args.commitToken || "");
        return { ok: true };
      default:
        throw Object.assign(new Error(`destination.${op}`), { code: "unsupported" });
    }
  } finally {
    await disposeRpc(dest);
  }
}

async function invokeSourceOpen(plugin, ctx, key) {
  if (typeof plugin.invokeSourceOpen === "function") {
    return plugin.invokeSourceOpen(ctx, key);
  }
  const src = await plugin.source(ctx);
  try {
    return await src.open(key);
  } finally {
    await disposeRpc(src);
  }
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
 * Typed factory context (`DestinationContext` / `SourceContext` /
 * `WorkerContext` / `IntegrationContext` / `ContentSourceContext`) from the
 * `x-bookclerk-context` header or the `context` body field.
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

async function handleRoleInvoke(request, env, url) {
  const plugin = env.PLUGIN;
  if (!plugin) {
    return errJson(null, "unavailable", "PLUGIN binding missing", 500);
  }

  if (request.method === "POST" && url.pathname === "/describe") {
    try {
      const result = await plugin.describe();
      return bridgeJson(result);
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/head") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      const meta = await invokeDest(plugin, "head", ctx, { key: body.key || "" });
      return bridgeJson({ found: meta != null, meta: meta ?? null });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/list") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      return bridgeJson(
        await invokeDest(plugin, "list", ctx, { options: body.options ?? body }),
      );
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

  if (request.method === "POST" && url.pathname === "/destination/copy") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      return bridgeJson(
        await invokeDest(plugin, "copy", ctx, { from: body.from, to: body.to }),
      );
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/delete") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      await invokeDest(plugin, "delete", ctx, { key: body.key || "" });
      return Response.json({ ok: true });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/commit") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      return bridgeJson(
        await invokeDest(plugin, "commit", ctx, {
          key: body.key || "",
          commitToken: body.commitToken || "",
        }),
      );
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/abortStage") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      await invokeDest(plugin, "abortStage", ctx, {
        key: body.key || "",
        commitToken: body.commitToken || "",
      });
      return Response.json({ ok: true });
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

  if (request.method === "POST" && url.pathname === "/worker/handle") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      const grantToken = body.grantToken;
      const invocation = body.invocation ?? {};
      const databases = body.databases ?? {};
      if (typeof plugin.invokeHandle === "function") {
        return bridgeJson(
          await plugin.invokeHandle(ctx, fromBridgeJson(invocation), grantToken, databases),
        );
      }
      const handler = await plugin.worker(ctx);
      try {
        throw Object.assign(new Error("granted reverse channel required"), {
          code: "internal",
        });
      } finally {
        await disposeRpc(handler);
      }
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/cliDescribe") {
    try {
      const schema = typeof plugin.cliDescribe === "function" ? await plugin.cliDescribe() : {};
      // Typed `CliSchema`; legacy guests that still return JSON text are parsed.
      return bridgeJson(typeof schema === "string" ? JSON.parse(schema || "{}") : schema ?? {});
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/cliInvoke") {
    try {
      const body = await request.json();
      const params = fromBridgeJson(body.params ?? {});
      if (typeof plugin.cliInvoke !== "function") {
        return errJson(null, "unsupported", "cliInvoke");
      }
      const result = await plugin.cliInvoke(params);
      // Typed `CliInvokeResult`; legacy guests that still return JSON text are parsed.
      return bridgeJson(typeof result === "string" ? JSON.parse(result || "{}") : result ?? {});
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/oidcClients") {
    try {
      const clients =
        typeof plugin.oidcClients === "function" ? await plugin.oidcClients() : [];
      return Response.json({ clients: Array.isArray(clients) ? clients : [] });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/databaseMigrations") {
    try {
      const body = await request.json().catch(() => ({}));
      const binding = typeof body.binding === "string" ? body.binding : "";
      const migrations =
        typeof plugin.databaseMigrations === "function"
          ? await plugin.databaseMigrations(binding)
          : [];
      return Response.json({
        migrations: requirePluginMigrationRegistration(migrations),
      });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  const roleMatch = url.pathname.match(/^\/(contentSource|integration)\/([^/]+)$/);
  if (roleMatch && request.method === "POST") {
    try {
      const role = roleMatch[1];
      const op = roleMatch[2];
      const body = await request.json();
      const ctx = contextFrom(request, body);
      const cap =
        role === "contentSource"
          ? await plugin.contentSource(ctx)
          : await plugin.integration(ctx);
      try {
        if (typeof cap[op] !== "function") {
          return errJson(null, "unsupported", `${role}.${op}`);
        }
        // `{ context, params }` for typed method params; `{ context, event }`
        // for `onEvent`. Methods without params receive no arguments.
        const args =
          body.params != null
            ? [fromBridgeJson(body.params)]
            : body.event != null
              ? [fromBridgeJson(body.event)]
              : [];
        const result = await cap[op](...args);
        if (op === "diagnose") {
          const lines = Array.isArray(result) ? result : Array.isArray(result?.lines) ? result.lines : [];
          return Response.json({ lines: lines.map((l) => String(l)) });
        }
        if (op === "pollEvents" && Array.isArray(result)) {
          return bridgeJson({ users: result });
        }
        return bridgeJson(result ?? { ok: true });
      } finally {
        await disposeRpc(cap);
      }
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
    return handleRoleInvoke(request, env, url);
  },
};
