// Worker entry point for the site audit (audit.mjs). It is entry.ts with one
// change: docs are read from a local server that audit.mjs runs over this
// checkout's docs/, instead of from the docs repository, so the audit needs
// no network and covers the docs as this commit has them. Never deployed.

import shell from "../generated/docs-shell.json" with { type: "json" };
import { DOCS_RAW_BASE, handle } from "../worker.ts";
import type { DocsFetch, Env } from "../worker.ts";

/** Bindings of the audit Worker. */
interface AuditEnv extends Env {
  /** Origin of audit.mjs's local docs server. */
  AUDIT_DOCS_ORIGIN: string;
}

export default {
  /**
   * Handles one request.
   *
   * @param request - The incoming request.
   * @param env - Bindings: the static assets and the origin of audit.mjs's
   *   local docs server.
   * @returns The response.
   */
  fetch(request, env) {
    /**
     * Sends a docs repository request to the local docs server instead.
     *
     * @param url - The docs repository URL.
     * @param init - Request options, passed on.
     * @returns The local server's answer.
     * @throws {Error} For any URL outside the docs repository.
     */
    const docsFetch: DocsFetch = (url, init) => {
      if (!url.startsWith(DOCS_RAW_BASE)) throw new Error(`audit Worker refused ${url}`);
      return fetch(`${env.AUDIT_DOCS_ORIGIN}/${url.slice(DOCS_RAW_BASE.length)}`, init);
    };
    return handle(request, env, { shell, fetch: docsFetch, cache: null });
  },
} satisfies ExportedHandler<AuditEnv>;
