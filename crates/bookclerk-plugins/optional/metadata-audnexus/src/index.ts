/**
 * Audnexus metadata and chapter provider (workerd guest).
 *
 * Exports the `metadata` entrypoint `plugin.toml` declares. The work happens
 * in `client.ts` and `audnexus.ts`; this module only adapts their typed
 * failures to `PluginError`. Audnexus has no title search, so `search` keeps
 * the SDK's `unsupported` answer.
 */

import {
  BookclerkEntrypoint,
  MetadataEntrypoint,
  PluginError,
  type HealthOk,
  type MetadataChapters,
  type MetadataChaptersParams,
  type MetadataLookupParams,
  type MetadataRecord,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { ProviderFailure } from "./audnexus.ts";
import { chapters, lookup, type ClientOptions } from "./client.ts";
import { VERSION } from "./version.ts";

/**
 * Runs `call`, turning a `ProviderFailure` into a `PluginError` with its code.
 *
 * @param call - The provider work.
 * @returns What `call` returns.
 * @throws {PluginError} When `call` fails.
 */
export async function guarded<T>(call: () => Promise<T>): Promise<T> {
  try {
    return await call();
  } catch (err) {
    if (err instanceof ProviderFailure) throw PluginError.fromWire(err.code, err.message);
    throw err;
  }
}

/** How the isolate reaches Audnexus. */
const OPTIONS: ClientOptions = { fetch: (url, init) => fetch(url, init), version: VERSION };

/** `metadata` entrypoint: book records and chapter layouts by ASIN. */
export class Metadata extends MetadataEntrypoint<Env> {
  /**
   * Reports ready without a request, so a probe never spends Audnexus's
   * capacity.
   *
   * @returns Always ready.
   */
  override health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }

  /**
   * Records for the ASINs among `params.identifiers`.
   *
   * @param params - Identifiers, marketplace and result cap.
   * @returns One record per known ASIN.
   */
  override lookup(params: MetadataLookupParams): Promise<MetadataRecord[]> {
    return guarded(() => lookup(OPTIONS, params));
  }

  /**
   * Chapter layout of the first ASIN among `params.identifiers`.
   *
   * @param params - Identifiers and marketplace.
   * @returns The layout, or `{ found: false }`.
   */
  override chapters(params: MetadataChaptersParams): Promise<MetadataChapters> {
    return guarded(() => chapters(OPTIONS, params));
  }
}

/** Identity the host merges over `plugin.toml`. */
export default class AudnexusPlugin extends BookclerkEntrypoint<Env> {
  /**
   * Describes the plugin.
   *
   * @returns Display name.
   */
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Audnexus" };
  }
}
