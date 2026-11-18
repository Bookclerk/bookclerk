import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/**
 * A promise the test settles itself.
 *
 * @returns The promise and its resolve function.
 */
function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/**
 * A JSON response the API helpers accept.
 *
 * @param body - The JSON body.
 * @returns The response.
 */
function json(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

beforeEach(() => {
  vi.resetModules();
  vi.stubGlobal("window", globalThis);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("signing out", () => {
  it("keeps no price asked while the sign-out request was still running", async () => {
    const logoutAnswer = deferred<Response>();
    const lookupAnswers: Array<(r: Response) => void> = [];
    const fetchMock = vi.fn((url: string) => {
      if (url === "/api/auth/logout") return logoutAnswer.promise;
      return new Promise<Response>((resolve) => lookupAnswers.push(resolve));
    });
    vi.stubGlobal("fetch", fetchMock);
    const api = await import("./api");
    const hint = { source: "libro", url: "https://example.test/a" };

    const signingOut = api.logout();
    // Asked after the first clear, while the old session cookie still works.
    const lookup = api.fetchPurchaseHints({ title: "A Book" });
    await vi.waitFor(() => expect(lookupAnswers).toHaveLength(1));

    logoutAnswer.resolve(json({}));
    await signingOut;
    lookupAnswers[0](json({ results: [{ hints: [hint], best: hint }] }));
    await expect(lookup).resolves.toMatchObject({ best: hint });

    // The next viewer asks again instead of reading the old answer.
    const again = api.fetchPurchaseHints({ title: "A Book" });
    await vi.waitFor(() => expect(lookupAnswers).toHaveLength(2));
    lookupAnswers[1](json({ results: [{ hints: [], best: null }] }));
    await expect(again).resolves.toMatchObject({ best: null });
  });
});
