import { describe, expect, it } from "vitest";
import { LatestRead } from "./latestRead";

describe("LatestRead", () => {
  it("applies reads that answer in the order they started", () => {
    const reads = new LatestRead();
    const first = reads.begin();
    const second = reads.begin();
    expect(reads.accept(first)).toBe(true);
    expect(reads.accept(second)).toBe(true);
  });

  it("drops an older read that answers after a newer one", () => {
    const reads = new LatestRead();
    const slow = reads.begin();
    const live = reads.begin();
    expect(reads.accept(live)).toBe(true);
    expect(reads.accept(slow)).toBe(false);
    // A read started after the newest applied one still applies.
    expect(reads.accept(reads.begin())).toBe(true);
  });

  it("applies each ticket at most once", () => {
    const reads = new LatestRead();
    const only = reads.begin();
    expect(reads.accept(only)).toBe(true);
    expect(reads.accept(only)).toBe(false);
  });
});
