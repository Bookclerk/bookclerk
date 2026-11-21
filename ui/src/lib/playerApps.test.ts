import { afterEach, describe, expect, it, vi } from "vitest";
import {
  answerPlayerConsent,
  createPlayerPassword,
  deletePlayerPassword,
  fetchPlayerConsent,
  isPlayerReturn,
  listPlayerPasswords,
  PLAYER_RETURN_PATH,
} from "./playerApps";

/**
 * Stubs `fetch` to answer every request with `body`.
 *
 * @param body - The JSON answer, or `null` for an empty answer.
 * @param status - The answer's status.
 * @returns The stub, which records each call.
 */
function stubFetch(body: unknown, status = 200) {
  const fetchMock = vi.fn(
    async (_url: string, _init?: RequestInit) =>
      new Response(body === null ? null : JSON.stringify(body), { status }),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("player passwords", () => {
  it("lists, makes and removes them", async () => {
    const view = { id: "pw1", label: "Phone", created_at: "2026-11-20T05:00:00Z" };
    let fetchMock = stubFetch({ passwords: [view] });
    expect(await listPlayerPasswords()).toEqual([view]);
    expect(fetchMock.mock.calls[0][0]).toBe("/api/auth/player-passwords");

    fetchMock = stubFetch({ password: view, secret: "ABCDE-FGHJK-LMNPQ-RSTUV" });
    const made = await createPlayerPassword("Phone");
    expect(made.secret).toBe("ABCDE-FGHJK-LMNPQ-RSTUV");
    const init = fetchMock.mock.calls[0][1];
    expect(init?.method).toBe("POST");
    expect(JSON.parse(String(init?.body))).toEqual({ label: "Phone" });

    fetchMock = stubFetch(null, 204);
    await deletePlayerPassword("pw/1");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/auth/player-passwords/pw%2F1");
    expect(fetchMock.mock.calls[0][1]?.method).toBe("DELETE");
  });

  it("treats an already removed password as removed", async () => {
    stubFetch(null, 404);
    await expect(deletePlayerPassword("gone")).resolves.toBeUndefined();
    stubFetch(null, 500);
    await expect(deletePlayerPassword("pw1")).rejects.toThrow("500");
  });
});

describe("the player consent page", () => {
  const search = "?redirect_uri=absorb%3A%2F%2Foauth&code_challenge=abc&state=xyz&other=dropped";

  it("sends only the request's own parameters", async () => {
    let fetchMock = stubFetch({ app_name: "Absorb" });
    expect((await fetchPlayerConsent(search)).app_name).toBe("Absorb");
    const url = new URL(String(fetchMock.mock.calls[0][0]), "https://books.example.com");
    expect(url.pathname).toBe("/api/auth/player-consent");
    expect([...url.searchParams.keys()]).toEqual(["redirect_uri", "code_challenge", "state"]);

    fetchMock = stubFetch({ redirect: PLAYER_RETURN_PATH });
    await answerPlayerConsent(search, true);
    expect(JSON.parse(String(fetchMock.mock.calls[0][1]?.body))).toEqual({
      redirect_uri: "absorb://oauth",
      code_challenge: "abc",
      state: "xyz",
      approve: true,
    });
  });

  it("goes only to the library's own return path", () => {
    const origin = "https://books.example.com";
    expect(isPlayerReturn(`${PLAYER_RETURN_PATH}?code=c`, origin)).toBe(true);
    expect(isPlayerReturn(`${origin}${PLAYER_RETURN_PATH}`, origin)).toBe(true);
    expect(isPlayerReturn(`https://evil.example${PLAYER_RETURN_PATH}`, origin)).toBe(false);
    expect(isPlayerReturn("absorb://oauth?code=c", origin)).toBe(false);
    expect(isPlayerReturn("/settings", origin)).toBe(false);
    expect(isPlayerReturn("http://[", origin)).toBe(false);
  });
});
