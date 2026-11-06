import { describe, expect, it } from "vitest";
import { ApiError, authMeUntilAnswered, fetchRetryingBusy, type AuthSession } from "./api";

const SIGNED_IN: AuthSession = {
  authenticated: true,
  role: "owner",
  default_view: "library",
  can_acquire: true,
  elevated: false,
};

function busy(retryAfter: string | null): Response {
  const headers = new Headers();
  if (retryAfter !== null) headers.set("Retry-After", retryAfter);
  return new Response("{}", { status: 503, headers });
}

describe("fetchRetryingBusy", () => {
  it("asks again while the daemon says to retry", async () => {
    const answers = [busy("2"), busy("1"), new Response("{}", { status: 200 })];
    const waits: number[] = [];
    const res = await fetchRetryingBusy(
      "/api/library/books",
      undefined,
      () => Promise.resolve(answers.shift() as Response),
      (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    );
    expect(res.status).toBe(200);
    expect(waits).toEqual([2000, 1000]);
  });

  it("does not retry a 503 without Retry-After or another error", async () => {
    for (const answer of [busy(null), new Response("{}", { status: 401 })]) {
      let calls = 0;
      const res = await fetchRetryingBusy(
        "/x",
        undefined,
        () => {
          calls += 1;
          return Promise.resolve(answer);
        },
        () => Promise.resolve(),
      );
      expect(res.status).toBe(answer.status);
      expect(calls).toBe(1);
    }
  });

  it("caps the wait and gives up after a few tries", async () => {
    let calls = 0;
    const waits: number[] = [];
    const res = await fetchRetryingBusy(
      "/x",
      undefined,
      () => {
        calls += 1;
        return Promise.resolve(busy("600"));
      },
      (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    );
    expect(res.status).toBe(503);
    expect(calls).toBe(5);
    expect(waits.every((ms) => ms === 10_000)).toBe(true);
  });
});

describe("authMeUntilAnswered", () => {
  it("keeps asking with backoff until the check answers", async () => {
    let calls = 0;
    const waits: number[] = [];
    const session = await authMeUntilAnswered(
      () => false,
      () => {
        calls += 1;
        const failures = [
          new ApiError(504, "Session check timed out"),
          new TypeError("Failed to fetch"),
          new ApiError(503, "busy"),
          new ApiError(502, "bad gateway"),
          new ApiError(504, "Session check timed out"),
        ];
        return calls <= failures.length
          ? Promise.reject(failures[calls - 1])
          : Promise.resolve(SIGNED_IN);
      },
      (ms) => {
        waits.push(ms);
        return Promise.resolve();
      },
    );
    expect(session).toEqual(SIGNED_IN);
    expect(waits).toEqual([1000, 2000, 4000, 8000, 10_000]);
  });

  it("stops when the caller no longer wants the answer", async () => {
    let stop = false;
    const session = await authMeUntilAnswered(
      () => stop,
      () => Promise.reject(new TypeError("Failed to fetch")),
      () => {
        stop = true;
        return Promise.resolve();
      },
    );
    expect(session).toBeNull();
  });

  it("gives up at once on an answer asking again cannot change", async () => {
    for (const failure of [new ApiError(410, "Your demo has ended."), new Error("bad JSON")]) {
      let calls = 0;
      await expect(
        authMeUntilAnswered(
          () => false,
          () => {
            calls += 1;
            return Promise.reject(failure);
          },
          () => Promise.resolve(),
        ),
      ).rejects.toBe(failure);
      expect(calls).toBe(1);
    }
  });
});
