// Worker entry point for the site audit (audit.ts). It is entry.ts with one
// change: docs are read from a local server that audit.ts runs over this
// checkout's docs/, instead of from the docs repository, so the audit needs
// no network and covers the docs as this commit has them. It also serves every
// catalog card at CATALOG_CARDS_PATH, which the deployed Worker never does,
// so the audit can lay out each record. Never deployed.

import shell from "../generated/docs-shell.json" with { type: "json" };
import { DOCS_RAW_BASE, handle } from "../worker.ts";
import type { DocsFetch, Env } from "../worker.ts";
import { catalogCard, catalogRecords } from "../catalog.ts";

/** Audit-only path answering every catalog card as one HTML fragment. */
const CATALOG_CARDS_PATH = "/__audit/catalog-cards";

/** Bindings of the audit Worker. */
interface AuditEnv extends Env {
  /** Origin of audit.ts's local docs server. */
  AUDIT_DOCS_ORIGIN: string;
}

export default {
  /**
   * Handles one request.
   *
   * @param request - The incoming request.
   * @param env - Bindings: the static assets and the origin of audit.ts's
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
    if (new URL(request.url).pathname === CATALOG_CARDS_PATH)
      return new Response(catalogRecords.map(catalogCard).join("\n"), {
        headers: { "Content-Type": "text/html; charset=utf-8" },
      });
    return handle(request, env, { shell, fetch: docsFetch, cache: null });
  },
} satisfies ExportedHandler<AuditEnv>;
