/**
 * Cover image fetcher (workerd guest).
 *
 * Exports the `covers` entrypoint `plugin.toml` declares. The work happens in
 * `fetcher.ts`; this module only adapts a malformed request to `PluginError`.
 */

import {
  BookclerkEntrypoint,
  CoverEntrypoint,
  PluginError,
  type CoverFetchParams,
  type CoverImage,
  type Destination,
  type HealthOk,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { CoverFailure, fetchCovers } from "./fetcher.ts";
import { VERSION } from "./version.ts";

/** `covers` entrypoint: downloads cover images into the host's sink. */
export class Covers extends CoverEntrypoint<Env> {
  /**
   * Reports ready without a request.
   *
   * @returns Always ready.
   */
  override health(): Promise<HealthOk> {
    return Promise.resolve({ ok: true, detail: "" });
  }

  /**
   * Fetches each URL and streams its body into `sink` under its index.
   *
   * @param params - URLs and the per-image byte cap.
   * @param sink - The host's destination for the bodies.
   * @returns One answer per URL, in request order.
   * @throws {PluginError} `invalid_params` when the request is malformed.
   */
  override async fetchCovers(params: CoverFetchParams, sink: Destination): Promise<CoverImage[]> {
    try {
      return await fetchCovers(
        { fetch: (url, init) => fetch(url, init), version: VERSION },
        params,
        sink,
      );
    } catch (err) {
      if (err instanceof CoverFailure) throw PluginError.fromWire(err.code, err.message);
      throw err;
    }
  }
}

/** Identity the host merges over `plugin.toml`. */
export default class CoversPlugin extends BookclerkEntrypoint<Env> {
  /**
   * Describes the plugin.
   *
   * @returns Display name.
   */
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Cover images" };
  }
}
