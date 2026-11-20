import { describe, expect, it } from "vitest";
import type { PortalPace } from "@/lib/api";
import { paceSummary } from "@/lib/pace";

/**
 * A store-default pace with nothing downloaded yet.
 *
 * @param overrides - Fields to change.
 * @returns The pace.
 */
function pace(overrides: Partial<PortalPace> = {}): PortalPace {
  return {
    mode: "default",
    downloads_per_day: 100,
    min_gap_seconds: 30,
    store_downloads_per_day: 100,
    store_min_gap_seconds: 30,
    custom_downloads_per_day: null,
    custom_min_gap_seconds: null,
    downloads_last_day: 0,
    cooldown_until: null,
    next_automatic_at: null,
    ...overrides,
  };
}

describe("paceSummary", () => {
  it("counts downloads against the daily budget and names the gap", () => {
    expect(paceSummary(pace({ downloads_last_day: 12 }))).toBe(
      "Automatic downloads: 12 of 100 in the last 24 hours · 30 s apart",
    );
  });

  it("says when there is no daily limit and no gap", () => {
    expect(
      paceSummary(pace({ mode: "unlimited", downloads_per_day: null, min_gap_seconds: 0 })),
    ).toBe("Automatic downloads: 0 in the last 24 hours, no daily limit");
  });

  it("names a store pause before the next download time", () => {
    const now = new Date("2026-11-19T10:00:00Z");
    const line = paceSummary(
      pace({
        cooldown_until: "2026-11-19T12:00:00Z",
        next_automatic_at: "2026-11-19T12:00:00Z",
      }),
      now,
    );
    expect(line).toContain("paused by the store until");
    expect(line).not.toContain("next at");
  });

  it("names the next automatic download when the budget is used up", () => {
    const now = new Date("2026-11-19T10:00:00Z");
    const line = paceSummary(
      pace({ downloads_last_day: 100, next_automatic_at: "2026-11-19T11:00:00Z" }),
      now,
    );
    expect(line).toContain("100 of 100");
    expect(line).toContain("next at");
  });
});
