// Worker entry point: binds the docs shell that `npm run build` writes to the
// request handler in worker.ts, which stays importable by the tests without
// a build.

import shell from "./generated/docs-shell.json" with { type: "json" };
import { handle } from "./worker.ts";
import type { Env } from "./worker.ts";

export default {
  /**
   * Handles one request.
   *
   * @param request - The incoming request.
   * @param env - Bindings: the static assets.
   * @param ctx - The request context.
   * @returns The response.
   */
  fetch(request, env, ctx) {
    return handle(request, env, { shell, cache: caches.default, waitUntil: (p) => ctx.waitUntil(p) });
  },
} satisfies ExportedHandler<Env>;
