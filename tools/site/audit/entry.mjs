// Worker entry point for the site audit (audit.mjs). It is entry.mjs with one
// change: docs are read from a local server that audit.mjs runs over this
// checkout's docs/, instead of from the docs repository, so the audit needs
// no network and covers the docs as this commit has them. Never deployed.

import shell from "../generated/docs-shell.json";
import { DOCS_RAW_BASE, handle } from "../worker.js";

export default {
  /**
   * Handles one request.
   *
   * @param {Request} request The incoming request.
   * @param {{ASSETS: {fetch: typeof fetch}, AUDIT_DOCS_ORIGIN: string}} env Bindings: the
   *   static assets and the origin of audit.mjs's local docs server.
   * @returns {Promise<Response>} The response.
   */
  fetch(request, env) {
    /**
     * Sends a docs repository request to the local docs server instead.
     *
     * @param {Request | URL | string} input The docs repository URL.
     * @param {RequestInit} [init] Request options, passed on.
     * @returns {Promise<Response>} The local server's answer.
     * @throws {Error} For any URL outside the docs repository.
     */
    const docsFetch = (input, init) => {
      const url = input instanceof Request ? input.url : String(input);
      if (!url.startsWith(DOCS_RAW_BASE)) throw new Error(`audit Worker refused ${url}`);
      return fetch(`${env.AUDIT_DOCS_ORIGIN}/${url.slice(DOCS_RAW_BASE.length)}`, init);
    };
    return handle(request, env, { shell, fetch: docsFetch, cache: null });
  },
};
