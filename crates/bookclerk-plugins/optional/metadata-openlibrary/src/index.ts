/**
 * Open Library metadata provider (workerd guest).
 *
 * Exports the `metadata` entrypoint `plugin.toml` declares. The work happens
 * in `client.ts` and `openlibrary.ts`; this module only adapts their typed
 * failures to `PluginError`.
 */

import {
  BookclerkEntrypoint,
  MetadataEntrypoint,
  PluginError,
  type HealthOk,
  type MetadataLookupParams,
  type MetadataRecord,
  type MetadataSearchParams,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { lookup, search, type ClientOptions } from "./client.ts";
import { ProviderFailure } from "./openlibrary.ts";
import { VERSION } from "./version.ts";

/**
 * Runs `call`, turning a `ProviderFailure` into a `PluginError` with its code.
 *
 * @param call - The provider work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
async function guarded<T>(call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (err) {
    if (err instanceof ProviderFailure) throw PluginError.fromWire(err.code, err.message);
    throw err;
  }
}

/** `metadata` entrypoint: lookups and title searches against Open Library. */
export class Metadata extends MetadataEntrypoint<Env> {
  /**
   * How this isolate reaches Open Library.
   *
   * @returns Client options from the granted settings.
   */
  private options(): ClientOptions {
    return {
      fetch: (url, init) => fetch(url, init),
      version: VERSION,
      contactEmail: this.env.CONFIG.contact_email,
    };
  }

  /**
   * Reports ready without a request, so a probe never spends Open Library's
   * rate budget.
   *
   * @returns Always ready.
   */
  override health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }

  /**
   * Records for an Open Library edition or work id, or an ISBN.
   *
   * @param params - Identifiers, marketplace (ignored) and result cap.
   * @returns Matching records.
   */
  override lookup(params: MetadataLookupParams): Promise<MetadataRecord[]> {
    return guarded(() => lookup(this.options(), params));
  }

  /**
   * Records whose title overlaps `params.title`.
   *
   * @param params - Title, author, language and result cap.
   * @returns Matching records.
   */
  override search(params: MetadataSearchParams): Promise<MetadataRecord[]> {
    return guarded(() => search(this.options(), params));
  }
}

/** Identity the host merges over `plugin.toml`. */
export default class OpenLibraryPlugin extends BookclerkEntrypoint<Env> {
  /**
   * Describes the plugin.
   *
   * @returns Display name.
   */
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Open Library" };
  }
}
