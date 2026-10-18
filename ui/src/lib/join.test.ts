import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConfigNodeView } from "./api";
import {
  answerBranchConsent,
  clerkNodes,
  authorizeParams,
  fingerprintGroups,
  formatCountdown,
  isClerkJoined,
  isConsentRedirect,
  JoinApiError,
  joinErrorMessage,
  looksLikeBranchInvite,
  parseJoinCallback,
} from "./join";
import { isAppPath, joinPage } from "./routes";

const AUTHORIZE =
  "?client_id=bookclerk-branch&response_type=code&scope=node%3Aenroll" +
  "&redirect_uri=http%3A%2F%2Flocalhost%3A8787%2Fjoin%2Fcallback" +
  "&code_challenge=abc&code_challenge_method=S256&state=s1&branch_name=Den+PC";

describe("join pages", () => {
  it("are app routes", () => {
    expect(isAppPath("/join/approve")).toBe(true);
    expect(isAppPath("/join/callback/")).toBe(true);
    expect(joinPage("/join/approve")).toBe("approve");
    expect(joinPage("/join/callback")).toBe("callback");
    expect(joinPage("/settings")).toBeNull();
  });
});

describe("parseJoinCallback", () => {
  it("reads code and state, and a denial", () => {
    expect(parseJoinCallback("?code=c1&state=s1")).toEqual({
      code: "c1",
      state: "s1",
      error: null,
      connection: "branch",
    });
    expect(parseJoinCallback("?error=access_denied&state=s1")).toEqual({
      code: "",
      state: "s1",
      error: "access_denied",
      connection: "branch",
    });
  });

  it("relays a clerk approval, and nothing else as one", () => {
    expect(parseJoinCallback("?code=c1&state=s1&connection=clerk")?.connection).toBe("clerk");
    expect(parseJoinCallback("?code=c1&state=s1&connection=owner")?.connection).toBe("branch");
  });

  it("needs a state", () => {
    expect(parseJoinCallback("?code=c1")).toBeNull();
    expect(parseJoinCallback("")).toBeNull();
  });
});

describe("authorizeParams", () => {
  it("keeps the first of a repeated name", () => {
    const params = authorizeParams(`${AUTHORIZE}&state=s2`);
    expect(params.state).toBe("s1");
    expect(params.branch_name).toBe("Den PC");
    expect(params.redirect_uri).toBe("http://localhost:8787/join/callback");
  });
});

describe("isConsentRedirect", () => {
  it("allows only the request's own redirect URI", () => {
    expect(
      isConsentRedirect("http://localhost:8787/join/callback?code=x&state=s1", AUTHORIZE),
    ).toBe(true);
    expect(isConsentRedirect("https://evil.example/join/callback?code=x", AUTHORIZE)).toBe(false);
    expect(isConsentRedirect("http://localhost:8787/other?code=x", AUTHORIZE)).toBe(false);
    expect(isConsentRedirect("not a url", AUTHORIZE)).toBe(false);
    expect(isConsentRedirect("http://localhost:8787/join/callback", "?state=s1")).toBe(false);
  });
});

describe("looksLikeBranchInvite", () => {
  it("accepts an enroll link with a bcn1 fragment", () => {
    expect(looksLikeBranchInvite(" https://library.example/enroll#bcn1.abc.def ")).toBe(true);
    expect(looksLikeBranchInvite("http://localhost:8787/enroll#bcn1.abc.def")).toBe(true);
  });

  it("refuses anything else", () => {
    expect(looksLikeBranchInvite("https://library.example/enroll")).toBe(false);
    expect(looksLikeBranchInvite("https://library.example/other#bcn1.a")).toBe(false);
    expect(looksLikeBranchInvite("bcn1.abc.def")).toBe(false);
    expect(looksLikeBranchInvite("javascript:alert(1)//enroll#bcn1.")).toBe(false);
  });
});

describe("formatCountdown", () => {
  const now = Date.parse("2026-10-18T12:00:00Z");
  it("counts down in m:ss and h:mm:ss", () => {
    expect(formatCountdown("2026-10-18T12:01:05Z", now)).toBe("1:05");
    expect(formatCountdown("2026-10-19T12:00:00Z", now)).toBe("24:00:00");
    expect(formatCountdown(now + 9_500, now)).toBe("0:09");
  });

  it("stops at zero and ignores bad input", () => {
    expect(formatCountdown("2026-10-18T11:00:00Z", now)).toBe("0:00");
    expect(formatCountdown("not a date", now)).toBe("");
  });
});

describe("fingerprintGroups", () => {
  it("drops the prefix and groups", () => {
    expect(fingerprintGroups("sha256:abcdefghij", 4)).toEqual(["abcd", "efgh", "ij"]);
  });
});

describe("joinErrorMessage", () => {
  it("words the daemon's refusals", () => {
    expect(joinErrorMessage(new JoinApiError(403, "x", "enrollment_disabled"), "f")).toBe(
      "This library isn't accepting new branches.",
    );
    expect(joinErrorMessage(new JoinApiError(410, "x", "join_expired"), "f")).toMatch(/Start again/);
    expect(joinErrorMessage(new JoinApiError(409, "use this address", "origin_mismatch"), "f")).toBe(
      "use this address",
    );
    expect(joinErrorMessage("nope", "fallback")).toBe("fallback");
  });

  it("keeps the library's own address on a mismatch", () => {
    const err = new JoinApiError(409, "m", "origin_mismatch", "https://library.example");
    expect(err.origin).toBe("https://library.example");
    expect(err.status).toBe(409);
  });
});

describe("answerBranchConsent", () => {
  afterEach(() => vi.unstubAllGlobals());

  function stubFetch() {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ redirect: "http://localhost:8787/join/callback" }), {
          status: 200,
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    return fetchMock;
  }

  it("sends a branch approval with no connection", async () => {
    const fetchMock = stubFetch();
    await answerBranchConsent(AUTHORIZE, true);
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body.approve).toBe(true);
    expect(body).not.toHaveProperty("connection");
    expect(body).not.toHaveProperty("step_up");
  });

  it("sends a clerk approval with its step-up, and never on a denial", async () => {
    const fetchMock = stubFetch();
    const stepUp = { method: "totp" as const, code: "123456" };
    await answerBranchConsent(AUTHORIZE, true, { step_up: stepUp });
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body.connection).toBe("clerk");
    expect(body.step_up).toEqual(stepUp);
    await answerBranchConsent(AUTHORIZE, false, { step_up: stepUp });
    const denied = JSON.parse(String(fetchMock.mock.calls[1][1].body));
    expect(denied.approve).toBe(false);
    expect(denied).not.toHaveProperty("step_up");
  });
});

describe("isClerkJoined", () => {
  it("tells a clerk answer from a branch's", () => {
    expect(
      isClerkJoined({
        connection: "clerk",
        cluster_id: "c",
        node_id: "n",
        library_name: "Home",
        database: "postgres",
        master_key_wrapped: true,
        restarting: true,
      }),
    ).toBe(true);
    expect(
      isClerkJoined({
        cluster: {
          cluster_id: "c",
          owner_origin: "https://library.example",
          node_id: "n",
          pinned: false,
          state: "active",
          enrolled_at: "2026-01-01T00:00:00Z",
        },
        library_name: "Home",
        running: true,
      }),
    ).toBe(false);
  });
});

describe("clerkNodes", () => {
  it("lists owner nodes only, this server first", () => {
    const node = (node_id: string, kind: "owner" | "host", this_node = false) =>
      ({ node_id, kind, this_node }) as ConfigNodeView;
    expect(
      clerkNodes([node("b", "owner"), node("h", "host"), node("a", "owner", true)]).map(
        (n) => n.node_id,
      ),
    ).toEqual(["a", "b"]);
  });
});
