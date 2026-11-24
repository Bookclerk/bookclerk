/**
 * Public media downloader (workerd guest).
 *
 * Exports the `mediaDownload` entrypoint `plugin.toml` declares. The work happens in
 * `fetcher.ts`; this module only adapts a malformed request to `PluginError`.
 */

import {
  BookclerkEntrypoint,
  MediaDownloadEntrypoint,
  PluginError,
  type MediaDownloadParams,
  type DownloadResult,
  type Destination,
  type HealthOk,
  type PluginDescribe,
} from "@bookclerk/plugin-sdk/workerd";
import type { Env } from "../bookclerk-configuration.js";
import { DownloadFailure, downloadMedia } from "./fetcher.ts";
import { VERSION } from "./version.ts";

/**
 * `mediaDownload` entrypoint: downloads cover images, or a store's audio
 * samples, into the host's sink.
 */
export class MediaDownload extends MediaDownloadEntrypoint<Env> {
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
   * @param params - URLs, the per-body byte cap, and what they name.
   * @param sink - The host's destination for the bodies.
   * @returns One answer per URL, in request order.
   * @throws {PluginError} `invalid_params` when the request is malformed.
   */
  override async downloadMedia(
    params: MediaDownloadParams,
    sink: Destination,
  ): Promise<DownloadResult[]> {
    try {
      return await downloadMedia(
        { fetch: (url, init) => fetch(url, init), version: VERSION },
        params,
        sink,
      );
    } catch (err) {
      if (err instanceof DownloadFailure) throw PluginError.fromWire(err.code, err.message);
      throw err;
    }
  }
}

/** Identity the host merges over `plugin.toml`. */
export default class MediaDownloadPlugin extends BookclerkEntrypoint<Env> {
  /**
   * Describes the plugin.
   *
   * @returns Display name.
   */
  override async describe(): Promise<PluginDescribe> {
    return { displayName: "Media downloads" };
  }
}
