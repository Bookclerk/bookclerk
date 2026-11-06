import { describe, expect, it } from "vitest";
import { COVER_RETRY_MAX_MS, MAX_COVER_RETRIES, coverRetryDelay } from "./coverRetry";

describe("coverRetryDelay", () => {
  it("doubles from five seconds", () => {
    expect([0, 1, 2, 3].map(coverRetryDelay)).toEqual([5_000, 10_000, 20_000, 40_000]);
  });

  it("gives up after the last retry", () => {
    expect(coverRetryDelay(MAX_COVER_RETRIES)).toBeNull();
    expect(coverRetryDelay(MAX_COVER_RETRIES + 3)).toBeNull();
  });

  it("refuses nonsense and never exceeds the cap", () => {
    expect(coverRetryDelay(-1)).toBeNull();
    expect(coverRetryDelay(1.5)).toBeNull();
    for (let retry = 0; retry < MAX_COVER_RETRIES; retry += 1) {
      expect(coverRetryDelay(retry)).toBeLessThanOrEqual(COVER_RETRY_MAX_MS);
    }
  });
});
