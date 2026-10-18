import { describe, expect, it } from "vitest";
import {
  authorizeParams,
  branchSharingSummary,
  canChangeBranchSharing,
  branchTitlesNote,
  fingerprintGroups,
  formatCountdown,
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
    });
    expect(parseJoinCallback("?error=access_denied&state=s1")).toEqual({
      code: "",
      state: "s1",
      error: "access_denied",
    });
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

describe("branch sharing (branches of branches)", () => {
  const row = {
    node_id: "n1",
    created_at: "2026-10-01T00:00:00Z",
    owner_user_id: 2,
    owner_name: "Riley",
    joined: true,
    share_books: true as boolean | null,
    include_branches: true as boolean | null,
    titles: { shared: 0, delivered: 0, detached: 0 },
  };

  it("summarises what a branch shares", () => {
    expect(branchSharingSummary(row)).toBe("Shares its books and its own branches' books");
    expect(branchSharingSummary({ ...row, include_branches: false })).toBe(
      "Shares its own books only",
    );
    expect(branchSharingSummary({ ...row, share_books: false })).toBe("Does not share its books");
    expect(branchSharingSummary({ ...row, share_books: null })).toBe("Has not shared books yet");
    expect(branchSharingSummary({ ...row, joined: false })).toBe("Left this library");
  });

  it("notes titles, and those from a branch that left", () => {
    expect(branchTitlesNote(row.titles, true)).toBeNull();
    expect(branchTitlesNote({ shared: 3, delivered: 2, detached: 1 }, true)).toBe(
      "3 books shared, 2 arrived · 1 book no longer shared",
    );
    expect(branchTitlesNote({ shared: 0, delivered: 0, detached: 2 }, false)).toBe(
      "2 books from a branch that left",
    );
  });
});

describe("canChangeBranchSharing", () => {
  it("is the operator or an Owner", () => {
    expect(canChangeBranchSharing("operator")).toBe(true);
    expect(canChangeBranchSharing("owner")).toBe(true);
    // The operator acting as an Owner acts as that Owner.
    expect(canChangeBranchSharing("owner", true)).toBe(true);
  });

  it("is nobody else", () => {
    expect(canChangeBranchSharing("operator", true)).toBe(false);
    expect(canChangeBranchSharing("administrator")).toBe(false);
    expect(canChangeBranchSharing("member")).toBe(false);
    expect(canChangeBranchSharing("host")).toBe(false);
    expect(canChangeBranchSharing("")).toBe(false);
  });
});
