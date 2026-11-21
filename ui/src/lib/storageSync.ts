/**
 * Labels for how a storage destination follows changes made outside
 * Bookclerk (its change feed and scheduled listings).
 *
 * @module
 */
import type { StorageSyncView } from "@/lib/api";

/** A badge's short label, whether it needs attention, and its tooltip lines. */
export interface StorageSyncSummary {
  /** Short badge text, such as `Feed healthy`. */
  label: string;
  /** True when the operator should look: the feed failed or missed changes, or the last listing failed or was held back. */
  trouble: boolean;
  /** Tooltip lines: file count, listing times and any errors. */
  details: string[];
}

/**
 * Summarizes a destination's sync state for its Settings badge.
 *
 * @param sync - The destination's sync view from the API.
 * @param when - Formats an RFC 3339 time for display.
 * @returns The badge's label, attention flag and tooltip lines.
 */
export function summarizeStorageSync(
  sync: StorageSyncView,
  when: (at: string) => string = (at) => new Date(at).toLocaleString(),
): StorageSyncSummary {
  const time = (at: string | undefined) => (at ? when(at) : "never");
  const details = [
    `${sync.objects} files`,
    `Last listing: ${time(sync.listed_at)}`,
    sync.listing_in_progress
      ? "A listing is in progress"
      : `Next listing: ${time(sync.next_listing_at)}`,
  ];
  if (sync.feed_error) details.push(`Feed error: ${sync.feed_error}`);
  if (sync.listing_error) details.push(sync.listing_error);
  if (sync.listing_error) return { label: "Listing error", trouble: true, details };
  switch (sync.feed_state) {
    case "failing":
      return { label: "Feed failing", trouble: true, details };
    case "gap":
      return { label: "Feed gap", trouble: true, details };
    case "healthy":
      return { label: "Feed healthy", trouble: false, details };
    case "off":
      return { label: sync.listed_at ? "Listed" : "Not listed yet", trouble: false, details };
  }
}
