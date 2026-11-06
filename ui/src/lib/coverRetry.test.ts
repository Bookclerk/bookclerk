import { describe, expect, it } from "vitest";
import {
  COVER_RETRY_JITTER,
  COVER_RETRY_MAX_MS,
  MAX_COVER_RETRIES,
  coverRetryDelay,
} from "./coverRetry";

const MIDDLE = () => 0.5;

describe("coverRetryDelay", () => {
  it("doubles from five seconds", () => {
    expect([0, 1, 2, 3].map((retry) => coverRetryDelay(retry, MIDDLE))).toEqual([
      5_000, 10_000, 20_000, 40_000,
    ]);
  });

  it("spreads each wait so failed covers do not retry together", () => {
    expect(coverRetryDelay(0, () => 0)).toBe(5_000 * (1 - COVER_RETRY_JITTER));
    expect(coverRetryDelay(0, () => 0.999_999)).toBeCloseTo(5_000 * (1 + COVER_RETRY_JITTER), -1);
  });

  it("gives up after the last retry", () => {
    expect(coverRetryDelay(MAX_COVER_RETRIES)).toBeNull();
    expect(coverRetryDelay(MAX_COVER_RETRIES + 3)).toBeNull();
  });

  it("refuses nonsense and never exceeds the cap", () => {
    expect(coverRetryDelay(-1)).toBeNull();
    expect(coverRetryDelay(1.5)).toBeNull();
    for (let retry = 0; retry < MAX_COVER_RETRIES; retry += 1) {
      expect(coverRetryDelay(retry, () => 0.999_999)).toBeLessThanOrEqual(COVER_RETRY_MAX_MS);
    }
  });
});
