// Writes sitemap.xml (https://www.sitemaps.org/protocol.html). build.ts
// writes one at deploy time from the pages and sidebar it knows, without
// dates; worker.ts answers /sitemap.xml with every page of the default docs
// version, dated from its last-updated.json, and falls back to the built
// file when that cannot be read.

import type { IsoDate } from "./last-updated.ts";
import { escapeHtml } from "./render.ts";

/** Most URLs one sitemap file may list, per the protocol. */
export const MAX_SITEMAP_URLS = 50_000;

/** One URL in the sitemap. */
export interface SitemapEntry {
  /** The page's URL path, starting with `/`. */
  path: string;
  /** When the page last changed, or null when unknown (no `<lastmod>`). */
  lastmod: IsoDate | null;
}

/**
 * Writes a sitemap.
 *
 * @param origin - The site's origin, such as `https://bookclerk.app`.
 * @param entries - The pages, in the order to list them. Only the first
 *   MAX_SITEMAP_URLS are written.
 * @returns The XML document.
 */
export function sitemapXml(origin: string, entries: readonly SitemapEntry[]): string {
  const urls = entries.slice(0, MAX_SITEMAP_URLS).map(({ path, lastmod }) => {
    const date = lastmod === null ? "" : `<lastmod>${lastmod}</lastmod>`;
    return `  <url><loc>${escapeHtml(origin + path)}</loc>${date}</url>`;
  });
  return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${urls.join("\n")}\n</urlset>\n`;
}
