/**
 * Loads the images of the Library, Wishlist and Accounts pages into the
 * browser's cache in the background, so switching to one of those pages
 * shows its thumbnails at once instead of asking for each one then.
 *
 * Covers and store icons are answered with a year-long `immutable` cache
 * header, so an image loaded here is reused when its page renders the same
 * URL. Each URL is the one the page itself renders, at the size it renders
 * (128 px covers; store icons as their `brand.logo`).
 *
 * The warm starts once per signed-in session, a few seconds after sign-in
 * and once the first page's own covers have loaded, and yields to any cover
 * on screen before each image (see {@link coversSettled}), so it never
 * competes with what the visitor is looking at. On the hosted demo it runs while the visitor is on
 * Discover, the page a new session opens on.
 *
 * @module
 */

import {
  coverSrc,
  fetchRequestQueue,
  fetchWishlist,
  fetchWorks,
  portalConnections,
  portalSources,
} from "@/lib/api";
import { coversSettled } from "@/lib/coverLoads";

/** Library rows warmed: the first page the Library opens on. */
export const WARM_LIBRARY_ROWS = 40;

/** Images loading at once while warming. */
export const WARM_CONCURRENCY = 2;

/**
 * Wait after sign-in before looking at the first page's covers, in ms: the
 * page has to fetch its rows and render its covers before
 * {@link coversSettled} can see them, and until then it reports nothing
 * loading.
 */
export const START_DELAY_MS = 3000;

/** Longest wait for the first page's own covers before the warm starts, in ms. */
const START_WAIT_MS = 8000;

/** Longest wait for on-screen covers before each warmed image, in ms. */
const YIELD_WAIT_MS = 2000;

/** What the three pages would show first, as read from the API. */
export interface PageImageSources {
  /** `cover_path` of each Library row on the first page, in page order. */
  libraryCovers: (string | null | undefined)[];
  /** `cover_path` of each of the caller's wishlist rows. */
  wishlistCovers: (string | null | undefined)[];
  /** `cover_path` of each row of the shared request queue. */
  queueCovers: (string | null | undefined)[];
  /** `brand.logo` of each connectable storefront and each connection. */
  storeLogos: (string | null | undefined)[];
}

/**
 * The same-origin image URLs to warm, without duplicates.
 *
 * Covers get the 128 px size every one of those pages renders; store logos
 * are kept only when they are same-origin paths (the API hands out
 * `/api/…` paths for them). Order: store logos (few, and Accounts shows
 * nothing else), wishlist, queue, then Library.
 *
 * @param sources - Paths read from the pages' API routes.
 * @returns URLs in load order.
 */
export function pageImageUrls(sources: PageImageSources): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  const add = (url: string | null) => {
    if (url && !seen.has(url)) {
      seen.add(url);
      out.push(url);
    }
  };
  for (const logo of sources.storeLogos) {
    const trimmed = logo?.trim();
    add(trimmed && trimmed.startsWith("/") && !trimmed.startsWith("//") ? trimmed : null);
  }
  for (const path of [
    ...sources.wishlistCovers,
    ...sources.queueCovers,
    ...sources.libraryCovers,
  ]) {
    add(coverSrc(path, 128));
  }
  return out;
}

/**
 * Loads each URL, a few at a time, waiting before each one until no cover
 * on screen is loading.
 *
 * @param urls - Image URLs, in load order.
 * @param load - Loads one image; resolves when it loaded or failed.
 * @param settle - Waits until on-screen covers have loaded (or a cap passed).
 * @param concurrency - Images loading at once.
 * @param cancelled - True once the warm should stop (sign-out).
 * @returns Resolves once every URL was tried or the warm was cancelled.
 */
export async function warmImages(
  urls: string[],
  load: (url: string) => Promise<void>,
  settle: () => Promise<void>,
  concurrency: number,
  cancelled: () => boolean,
): Promise<void> {
  let next = 0;
  const lane = async () => {
    while (!cancelled() && next < urls.length) {
      const url = urls[next++];
      await settle();
      if (cancelled()) return;
      await load(url);
    }
  };
  await Promise.all(Array.from({ length: Math.max(1, concurrency) }, lane));
}

/**
 * Loads an image into the browser's cache at low priority.
 *
 * @param url - Same-origin image URL.
 * @returns Resolves when it loaded or failed.
 */
function loadImage(url: string): Promise<void> {
  return new Promise((resolve) => {
    const img = new Image();
    img.decoding = "async";
    img.fetchPriority = "low";
    img.onload = () => resolve();
    img.onerror = () => resolve();
    img.src = url;
  });
}

/**
 * The value of a settled read, or `[]` when it failed (the caller may not
 * see that page, or it is not available on this node).
 *
 * @param result - A settled read.
 * @returns The rows, or an empty list.
 */
function rows<T>(result: PromiseSettledResult<T[]>): T[] {
  return result.status === "fulfilled" ? result.value : [];
}

/**
 * Reads what the Library, Wishlist and Accounts pages show first and warms
 * their images. Never throws: a read the caller may not make is skipped.
 *
 * @param cancelled - True once the warm should stop (sign-out).
 * @param startDelayMs - Wait before looking at the first page's covers.
 * @returns Resolves when the warm is done or cancelled.
 */
export async function warmPageImages(
  cancelled: () => boolean,
  startDelayMs: number = START_DELAY_MS,
): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, startDelayMs));
  if (cancelled()) return;
  await coversSettled(START_WAIT_MS);
  if (cancelled()) return;
  const [works, wishlist, queue, sources, connections] = await Promise.allSettled([
    fetchWorks({ sort: "title", status: "all", limit: WARM_LIBRARY_ROWS }).then((p) => p.works),
    fetchWishlist(),
    fetchRequestQueue(),
    portalSources(),
    portalConnections(),
  ]);
  if (cancelled()) return;
  const urls = pageImageUrls({
    libraryCovers: rows(works).map((w) => w.cover_path),
    wishlistCovers: rows(wishlist).map((t) => t.cover_path),
    queueCovers: rows(queue).map((t) => t.cover_path),
    storeLogos: [
      ...rows(sources).map((s) => s.brand.logo),
      ...rows(connections).map((c) => c.brand?.logo),
    ],
  });
  await warmImages(
    urls,
    loadImage,
    () => coversSettled(YIELD_WAIT_MS),
    WARM_CONCURRENCY,
    cancelled,
  );
}
