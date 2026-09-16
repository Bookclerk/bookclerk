/**
 * Bookclerk bridge worker — HTTP ↔ Workers RPC service binding.
 *
 * Role routes (`/describe`, `/destination/*`, …): one invocation envelope per
 * HTTP request. The bridge creates the role capability, invokes the method,
 * and disposes the stub before completing. No dest-id table is retained
 * across requests.
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

function contextFrom(request, body) {
  const header = request.headers.get("x-bookclerk-context");
  if (header) {
    try {
      return JSON.parse(header);
    } catch {
      return { json: header };
    }
  }
  if (body && typeof body === "object") {
    if (body.context && typeof body.context === "object") return body.context;
    if (typeof body.json === "string" || body.json === undefined) {
      return { json: body.json || "", jobId: body.jobId };
    }
  }
  return { json: "" };
}

async function handleRoleInvoke(request, env, url) {
  const plugin = env.PLUGIN;
  if (!plugin) {
    return errJson(null, "unavailable", "PLUGIN binding missing", 500);
  }

  if (request.method === "POST" && url.pathname === "/describe") {
    try {
      const result = await plugin.describe();
      return Response.json(result);
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
      return Response.json({ found: meta != null, meta: meta ?? null });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/list") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      return Response.json(
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
      return Response.json(result);
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/destination/copy") {
    try {
      const body = await request.json();
      const ctx = contextFrom(request, body);
      return Response.json(
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
      return Response.json(
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
        return Response.json(
          await plugin.invokeHandle(ctx, invocation, grantToken, databases),
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
      const json = typeof plugin.cliDescribe === "function" ? await plugin.cliDescribe() : "{}";
      return Response.json({ json: typeof json === "string" ? json : JSON.stringify(json) });
    } catch (err) {
      const { code, message } = catchErr(err);
      return errJson(null, code, message);
    }
  }

  if (request.method === "POST" && url.pathname === "/cliInvoke") {
    try {
      const body = await request.json();
      const paramsJson = body.paramsJson || JSON.stringify(body);
      if (typeof plugin.cliInvoke !== "function") {
        return errJson(null, "unsupported", "cliInvoke");
      }
      const json = await plugin.cliInvoke(paramsJson);
      return Response.json({ json: typeof json === "string" ? json : JSON.stringify(json) });
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
        const args = body.paramsJson != null ? [body.paramsJson] : body.event != null ? [body.event] : [];
        const result = await cap[op](...args);
        return Response.json(result ?? { ok: true });
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
