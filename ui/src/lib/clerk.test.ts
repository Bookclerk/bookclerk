import { afterEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "./api";
import {
  clerkCallbackRetryable,
  clerkCardSummary,
  clerkErrorMessage,
  clerkRequestFailure,
  fetchClerkCards,
  finishClerkSignIn,
  inspectCardLibrary,
  parseClerkCallback,
  requestFromLibrary,
  signOutClerkCard,
  startClerkSignIn,
  type ClerkCardView,
} from "./clerk";
import { JoinApiError, windowLibraries } from "./join";

const CARD: ClerkCardView = {
  library_id: "lib-1",
  origin: "https://books.example.com",
  library_name: "books.example.com",
  pinned: false,
  display_name: "Riley",
  clerk_name: "Study laptop",
  state: "active",
  signed_in_at: "2026-11-17T09:02:41Z",
};

/**
 * Stubs `fetch` to answer every request with `body`.
 *
 * @param body - The JSON answer.
 * @param status - The answer's status.
 * @returns The stub, which records each call.
 */
function stubFetch(body: unknown, status = 200) {
  const fetchMock = vi.fn(
    async (_url: string, _init: RequestInit) => new Response(JSON.stringify(body), { status }),
  );
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

/**
 * The JSON body a recorded call sent.
 *
 * @param fetchMock - The stub from {@link stubFetch}.
 * @param n - Which call, from 0.
 * @returns The parsed body, or `undefined` when the call sent none.
 */
function sent(fetchMock: ReturnType<typeof stubFetch>, n = 0): unknown {
  const body = fetchMock.mock.calls[n][1].body;
  return body === undefined ? undefined : JSON.parse(String(body));
}

describe("parseClerkCallback", () => {
  it("reads the code and state", () => {
    expect(parseClerkCallback("?code=c1&state=s1")).toEqual({
      code: "c1",
      state: "s1",
      error: null,
    });
  });

  it("keeps the library's refusal", () => {
    expect(parseClerkCallback("?error=access_denied&state=s1")).toEqual({
      code: "",
      state: "s1",
      error: "access_denied",
    });
  });

  it("needs a state", () => {
    expect(parseClerkCallback("?code=c1")).toBeNull();
    expect(parseClerkCallback("?code=c1&state=%20")).toBeNull();
    expect(parseClerkCallback("")).toBeNull();
  });
});

describe("card requests", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("list the cards", async () => {
    const fetchMock = stubFetch({ cards: [CARD], default_clerk_name: "study-mini" });
    const body = await fetchClerkCards();
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk");
    expect(fetchMock.mock.calls[0][1].method).toBe("GET");
    expect(body.cards[0].clerk_name).toBe("Study laptop");
  });

  it("read the library", async () => {
    const fetchMock = stubFetch({ library: { origin: "https://books.example.com" } });
    await inspectCardLibrary("books.example.com");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk/inspect");
    expect(sent(fetchMock)).toEqual({ address: "books.example.com" });
  });

  it("start a sign-in, leaving out what is not given", async () => {
    const fetchMock = stubFetch({ authorize_url: "https://books.example.com/oidc/authorize" });
    await startClerkSignIn({
      address: "https://books.example.com",
      fingerprint: null,
      clerk_name: "  ",
    });
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk/start");
    expect(sent(fetchMock)).toEqual({ address: "https://books.example.com" });
    await startClerkSignIn({
      address: "https://books.example.com",
      fingerprint: "kQ3n",
      clerk_name: " Study laptop ",
    });
    expect(sent(fetchMock, 1)).toEqual({
      address: "https://books.example.com",
      fingerprint: "kQ3n",
      clerk_name: "Study laptop",
    });
    // A card that replaces a branch names the branch, not an address.
    await startClerkSignIn({ from_branch: "lib-1", clerk_name: "" });
    expect(sent(fetchMock, 2)).toEqual({ from_branch: "lib-1" });
  });

  it("finish a sign-in with what the library sent back", async () => {
    const fetchMock = stubFetch({ card: CARD });
    const { card } = await finishClerkSignIn({ code: "c1", state: "s1", error: null });
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk/callback");
    expect(sent(fetchMock)).toEqual({ code: "c1", state: "s1" });
    expect(card.library_id).toBe("lib-1");
    await finishClerkSignIn({ code: "", state: "s1", error: "access_denied" });
    expect(sent(fetchMock, 1)).toEqual({ code: "", state: "s1", error: "access_denied" });
  });

  it("sign a card out", async () => {
    const fetchMock = stubFetch({ library_id: "lib-1", library_signed_out: false });
    const done = await signOutClerkCard("lib-1");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk/sign-out");
    expect(sent(fetchMock)).toEqual({ library_id: "lib-1" });
    expect(done.library_signed_out).toBe(false);
  });

  it("ask a library for a book its branches hold", async () => {
    const fetchMock = stubFetch({ library_id: "lib-1" });
    const sentBack = await requestFromLibrary("lib-1", "work-7");
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerk/request");
    expect(sent(fetchMock)).toEqual({ library_id: "lib-1", work_id: "work-7" });
    expect(sentBack.library_id).toBe("lib-1");
  });

  it("carry the daemon's refusal code", async () => {
    stubFetch({ error: "already_joined", detail: "already a branch" }, 409);
    const err = await startClerkSignIn({
      address: "https://books.example.com",
      fingerprint: null,
      clerk_name: "Laptop",
    }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(JoinApiError);
    expect(clerkErrorMessage(err, "x")).toMatch(/already a branch of that library/);
    expect(clerkCallbackRetryable(err)).toBe(false);
  });
});

describe("clerkRequestFailure", () => {
  it("says what the library answered", () => {
    const failed = (code: string) => clerkRequestFailure(new ApiError(400, "x", { code }));
    expect(failed("not_allowed")).toMatch(/hasn't allowed you/);
    expect(failed("not_listed")).toMatch(/No branch/);
    expect(failed("card_refused")).toMatch(/Sign in to it again/);
    expect(failed("library_busy")).toMatch(/busy/);
    expect(failed("not_found")).toMatch(/no longer holds a library card/);
    expect(failed("unreachable")).toMatch(/couldn't be reached/);
    expect(failed("library_error")).toBe("The library couldn't take the request.");
    expect(clerkRequestFailure(new TypeError("Failed to fetch"))).toBe(
      "The library couldn't take the request.",
    );
  });
});

describe("clerkErrorMessage", () => {
  it("words each refusal", () => {
    for (const code of [
      "already_joined",
      "join_expired",
      "denied",
      "refused",
      "transport_insecure",
      "fingerprint_unconfirmed",
      "not_found",
    ]) {
      const message = clerkErrorMessage(new ApiError(400, "raw", { code }), "fallback");
      expect(message).not.toBe("raw");
      expect(message).not.toBe("fallback");
    }
  });

  it("falls back to the server's words, then the caller's", () => {
    expect(
      clerkErrorMessage(new ApiError(502, "owner unreachable", { code: "unreachable" }), "f"),
    ).toBe("owner unreachable");
    expect(clerkErrorMessage(new Error("offline"), "f")).toBe("offline");
    expect(clerkErrorMessage("??", "f")).toBe("f");
  });
});

describe("clerkCallbackRetryable", () => {
  it("retries only a request that got no answer", () => {
    expect(clerkCallbackRetryable(new TypeError("Failed to fetch"))).toBe(true);
    expect(clerkCallbackRetryable(new ApiError(502, "unreachable", { code: "unreachable" }))).toBe(
      false,
    );
  });
});

describe("clerkCardSummary", () => {
  it("names the person and this computer", () => {
    expect(clerkCardSummary(CARD)).toBe('Riley, as "Study laptop"');
    expect(clerkCardSummary({ ...CARD, display_name: null })).toBe('Signed in as "Study laptop"');
    expect(clerkCardSummary({ ...CARD, state: "signed_out" })).toMatch(/library ended this card/);
  });
});

describe("windowLibraries", () => {
  it("lists window libraries, then card libraries, each host once", () => {
    const link = { owner_origin: "https://books.example.com" } as Parameters<
      typeof windowLibraries
    >[0]["libraries"][number];
    expect(
      windowLibraries({
        libraries: [link],
        cards: [CARD, { ...CARD, library_id: "lib-2", origin: "https://other.example:8443" }],
      }),
    ).toEqual(["books.example.com", "other.example:8443"]);
    expect(windowLibraries({ libraries: [], cards: [] })).toEqual([]);
    expect(windowLibraries({ libraries: [], cards: [{ ...CARD, origin: "not a url" }] })).toEqual([
      "not a url",
    ]);
  });
});
