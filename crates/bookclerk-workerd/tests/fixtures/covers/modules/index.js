/**
 * Contract fixture: `CoverFetcher.fetchCovers` puts a made-up body for each
 * URL into the host sink, all at once, without touching the network.
 *
 * - a URL ending in `/missing` is answered `stored: false`;
 * - a URL ending in `/big` puts a body one byte over `maxBytes`;
 * - a URL ending in `/twice` puts its body twice under the same key;
 * - any other URL puts `body:<index>` under its index.
 */

import { BookclerkEntrypoint, CoverEntrypoint } from "@bookclerk/plugin-sdk/workerd";

function bytesStream(text) {
  const bytes = typeof text === "string" ? new TextEncoder().encode(text) : text;
  return new ReadableStream({
    start(controller) {
      controller.enqueue(bytes);
      controller.close();
    },
  });
}

export class Covers extends CoverEntrypoint {
  async fetchCovers(params, sink) {
    return Promise.all(
      params.urls.map(async (url, i) => {
        if (url.endsWith("/missing")) return { url, stored: false, error: "HTTP 404" };
        try {
          if (url.endsWith("/big")) {
            await sink.put(String(i), bytesStream(new Uint8Array(params.maxBytes + 1)));
          } else if (url.endsWith("/twice")) {
            await sink.put(String(i), bytesStream("first"));
            await sink.put(String(i), bytesStream("second"));
          } else {
            await sink.put(String(i), bytesStream(`body:${i}`), { contentType: "image/png" });
          }
        } catch (err) {
          return { url, stored: false, error: String(err?.message ?? err) };
        }
        return { url, stored: true, contentType: "image/png" };
      }),
    );
  }
}

export default class CoversFixture extends BookclerkEntrypoint {}
