/**
 * Paging helper for a storefront's `scan`: turns the storefront's own
 * pages into `ScanPage` replies the host accepts.
 *
 * A store page can hold more titles than one reply may carry
 * (`MAX_SCAN_PAGE_BOOKS`). {@link scanPage} fetches the store page the
 * cursor names, returns the next slice of it, and writes a cursor that names
 * both the store page and the slice, so the plugin pages once and stays
 * stateless between calls.
 *
 * @module
 */

import { MAX_SCAN_CURSOR_BYTES, MAX_SCAN_PAGE_BOOKS } from "./abi.js";
import { PluginError } from "./errors.js";
import type { ScanBook, ScanPage, ScanParams } from "./generated.js";

/** Prefix of every cursor {@link scanPage} writes; bumps if the layout changes. */
const CURSOR_PREFIX = "sp1.";

/** Measures cursors the way the wire does: in UTF-8 bytes. */
const UTF8 = new TextEncoder();

/** One page of a storefront's own library listing, as the plugin fetched it. */
export interface StorePage {
  /** Titles on this store page, in the store's order; any number of them. */
  books: ScanBook[];
  /**
   * Product ids gone from the account since `ScanParams.syncToken`, on a
   * delta pass only; any number of them.
   */
  removed?: string[];
  /**
   * The storefront's own cursor for the store page after this one; empty or
   * absent when this is the last store page.
   */
  next?: string;
  /**
   * On the last store page: the token the host sends as
   * `ScanParams.syncToken` on the account's next pass. Ignored on earlier
   * pages.
   */
  syncToken?: string;
  /**
   * On the last store page: whether the pass listed every title the account
   * holds (a full pass). Ignored on earlier pages.
   */
  complete?: boolean;
  /** Storefront requests this fetch took; 1 when absent. */
  storePages?: number;
}

/**
 * Fetches one store page.
 *
 * @param storeCursor - The storefront's own cursor: empty for the first
 *   store page of a pass, otherwise a {@link StorePage.next} it returned.
 * @param params - The scan call's params (account, credential, sync token).
 * @returns That store page.
 */
export type FetchStorePage = (storeCursor: string, params: ScanParams) => Promise<StorePage>;

/** Where a {@link scanPage} cursor points: a store page and an offset into it. */
interface ScanPosition {
  /** The storefront's own cursor for the store page. */
  storeCursor: string;
  /** Index of the first title (and removed id) the reply carries. */
  offset: number;
}

/**
 * Cursor naming `position`, read back by {@link readCursor}.
 *
 * @param position - Store page and offset.
 * @returns The cursor, at most `MAX_SCAN_CURSOR_BYTES` UTF-8 bytes.
 * @throws {@link PluginError} `internal` when the store cursor is too long
 *   to fit.
 */
function writeCursor(position: ScanPosition): string {
  const cursor = `${CURSOR_PREFIX}${position.offset}.${position.storeCursor}`;
  if (UTF8.encode(cursor).length > MAX_SCAN_CURSOR_BYTES) {
    throw new PluginError(
      "internal",
      `store cursor too long: a scan cursor is at most ${MAX_SCAN_CURSOR_BYTES} bytes`,
    );
  }
  return cursor;
}

/**
 * Position a scan cursor names: the first store page for the empty cursor a
 * pass starts with, otherwise what {@link writeCursor} wrote.
 *
 * @param cursor - `ScanParams.cursor`.
 * @returns Store page and offset.
 * @throws {@link PluginError} `invalid_cursor` for a cursor this helper did
 *   not write, so the host restarts the pass.
 */
function readCursor(cursor: string): ScanPosition {
  if (cursor === "") return { storeCursor: "", offset: 0 };
  const stale = () => new PluginError("invalid_cursor", "scan cursor was not written by scanPage");
  if (!cursor.startsWith(CURSOR_PREFIX)) throw stale();
  const rest = cursor.slice(CURSOR_PREFIX.length);
  const dot = rest.indexOf(".");
  const digits = dot < 0 ? "" : rest.slice(0, dot);
  if (!/^(0|[1-9][0-9]{0,15})$/.test(digits)) throw stale();
  const offset = Number(digits);
  if (!Number.isSafeInteger(offset) || offset % MAX_SCAN_PAGE_BOOKS !== 0) throw stale();
  return { storeCursor: rest.slice(dot + 1), offset };
}

/**
 * Answers one `scan` call from the storefront's own pages.
 *
 * Fetches the store page `params.cursor` names and returns at most
 * `MAX_SCAN_PAGE_BOOKS` of its titles and removed ids. A store page larger
 * than that is returned over several calls, each fetching it again. The pass
 * ends on the last slice of the store page whose {@link StorePage.next} is
 * empty, which carries that page's `syncToken` and `complete`.
 *
 * @example
 * ```ts
 * scan(params: ScanParams): Promise<ScanPage> {
 *   return scanPage(params, async (storeCursor) => {
 *     const page = Number(storeCursor || "1");
 *     const listing = await this.library(params.accountId, page);
 *     return {
 *       books: listing.items.map(toScanBook),
 *       next: listing.hasMore ? String(page + 1) : "",
 *       complete: true,
 *     };
 *   });
 * }
 * ```
 *
 * @param params - The scan call's params.
 * @param fetchStorePage - Fetches one store page.
 * @returns The reply for this call.
 * @throws {@link PluginError} `invalid_cursor` when the cursor was not
 *   written by this helper or no longer fits the store page (the store
 *   changed mid-pass), so the host restarts the pass; `internal` when a
 *   store cursor is too long for a scan cursor.
 */
export async function scanPage(
  params: ScanParams,
  fetchStorePage: FetchStorePage,
): Promise<ScanPage> {
  const { storeCursor, offset } = readCursor(params.cursor);
  const page = await fetchStorePage(storeCursor, params);
  const removed = page.removed ?? [];
  if (offset > 0 && offset >= page.books.length && offset >= removed.length) {
    throw new PluginError("invalid_cursor", "the store page shrank during the pass");
  }
  const end = offset + MAX_SCAN_PAGE_BOOKS;
  const reply: ScanPage = {
    books: page.books.slice(offset, end),
    removed: removed.slice(offset, end),
    nextCursor: "",
    syncToken: "",
    complete: false,
    storePages: page.storePages ?? 1,
  };
  if (page.books.length > end || removed.length > end) {
    reply.nextCursor = writeCursor({ storeCursor, offset: end });
  } else if (page.next) {
    reply.nextCursor = writeCursor({ storeCursor: page.next, offset: 0 });
  } else {
    reply.syncToken = page.syncToken ?? "";
    reply.complete = page.complete ?? false;
  }
  return reply;
}
