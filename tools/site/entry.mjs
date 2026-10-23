// Worker entry point: binds the docs shell that `npm run build` writes to the
// request handler in worker.js, which stays importable by the tests without
// a build.

import shell from "./generated/docs-shell.json";
import { handle } from "./worker.js";

export default {
  /**
   * Handles one request.
   *
   * @param {Request} request The incoming request.
   * @param {{ASSETS: {fetch: typeof fetch}}} env Bindings: the static assets.
   * @param {{waitUntil: (promise: Promise<unknown>) => void}} ctx The request context.
   * @returns {Promise<Response>} The response.
   */
  fetch(request, env, ctx) {
    return handle(request, env, { shell, cache: caches.default, waitUntil: (p) => ctx.waitUntil(p) });
  },
};
