import { describe, expect, it } from "vitest";
import type { StorageSyncView } from "./api";
import { summarizeStorageSync } from "./storageSync";

/**
 * A sync view for the tests.
 *
 * @param over - Fields that differ from a healthy, listed destination.
 * @returns The view.
 */
function view(over: Partial<StorageSyncView> = {}): StorageSyncView {
  return {
    feed_state: "healthy",
    listed_at: "2026-11-20T18:00:00Z",
    next_listing_at: "2026-11-21T18:00:00Z",
    listing_in_progress: false,
    objects: 12,
    ...over,
  };
}

/**
 * Echoes the time, so the tests need no locale.
 *
 * @param at - An RFC 3339 time.
 * @returns The same text.
 */
const raw = (at: string) => at;

describe("summarizeStorageSync", () => {
  it("is calm when the feed is healthy", () => {
    const summary = summarizeStorageSync(view(), raw);
    expect(summary).toEqual({
      label: "Feed healthy",
      trouble: false,
      details: [
        "12 files",
        "Last listing: 2026-11-20T18:00:00Z",
        "Next listing: 2026-11-21T18:00:00Z",
      ],
    });
  });

  it("flags a failing feed or a gap", () => {
    const failing = summarizeStorageSync(
      view({ feed_state: "failing", feed_error: "denied" }),
      raw,
    );
    expect(failing.label).toBe("Feed failing");
    expect(failing.trouble).toBe(true);
    expect(failing.details).toContain("Feed error: denied");
    expect(summarizeStorageSync(view({ feed_state: "gap" }), raw).label).toBe("Feed gap");
  });

  it("puts a listing error first, whatever caused it", () => {
    const held = summarizeStorageSync(
      view({ feed_state: "off", listing_error: "kept 40 unlisted files" }),
      raw,
    );
    expect(held.label).toBe("Listing error");
    expect(held.trouble).toBe(true);
    expect(held.details).toContain("kept 40 unlisted files");
    const failed = summarizeStorageSync(view({ listing_error: "request timed out" }), raw);
    expect(failed.label).toBe("Listing error");
    expect(failed.details).toContain("request timed out");
  });

  it("says whether a feedless destination was listed", () => {
    expect(summarizeStorageSync(view({ feed_state: "off" }), raw).label).toBe("Listed");
    const fresh = summarizeStorageSync(
      view({ feed_state: "off", listed_at: undefined, listing_in_progress: true }),
      raw,
    );
    expect(fresh.label).toBe("Not listed yet");
    expect(fresh.details).toEqual(["12 files", "Last listing: never", "A listing is in progress"]);
  });

  it("formats times with the locale by default", () => {
    const summary = summarizeStorageSync(view());
    expect(summary.details[1]).toBe(
      `Last listing: ${new Date("2026-11-20T18:00:00Z").toLocaleString()}`,
    );
  });
});
