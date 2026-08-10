/**
 * Bookclerk bridge worker — HTTP ↔ Workers RPC service binding.
 *
 * bookclerk-workerd POSTs `{ id, method, params }` to `/rpc` and receives
 * `{ id, result }` or `{ id, error: { code, message } }`.
 */

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return new Response("ok", { status: 200 });
    }
    if (request.method !== "POST" || url.pathname !== "/rpc") {
      return new Response("not found", { status: 404 });
    }

    let body;
    try {
      body = await request.json();
    } catch {
      return Response.json(
        { error: { code: "invalid_params", message: "invalid JSON body" } },
        { status: 400 },
      );
    }

    const id = body?.id ?? null;
    const method = body?.method;
    if (typeof method !== "string" || !method) {
      return Response.json(
        {
          id,
          error: { code: "invalid_params", message: "method required" },
        },
        { status: 400 },
      );
    }

    const plugin = env.PLUGIN;
    if (!plugin || typeof plugin[method] !== "function") {
      return Response.json({
        id,
        error: {
          code: "unsupported",
          message: `method \`${method}\` not exported by plugin entrypoint`,
        },
      });
    }

    try {
      const params = body.params;
      const result =
        params === undefined || params === null
          ? await plugin[method]()
          : await plugin[method](params);
      return Response.json({ id, result: result ?? null });
    } catch (err) {
      const code =
        err && typeof err === "object" && typeof err.code === "string"
          ? err.code
          : "internal";
      const message =
        err instanceof Error
          ? err.message
          : typeof err === "string"
            ? err
            : String(err);
      return Response.json({
        id,
        error: { code, message },
      });
    }
  },
};
