import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConfigNodeView } from "./api";
import {
  answerBranchConsent,
  answerClientConsent,
  rememberApproveReturn,
  takeApproveReturn,
  clerkNodes,
  authorizeParams,
  branchSharingSummary,
  broughtConnectionsSummary,
  canChangeBranchSharing,
  connectionsAddedLine,
  branchTitlesNote,
  fingerprintGroups,
  formatCountdown,
  isClerkJoined,
  isConsentRedirect,
  jobShareSummary,
  JoinApiError,
  joinErrorMessage,
  listClerks,
  looksLikeBranchInvite,
  parseJoinCallback,
  windowEntryFormats,
  setLinkDownload,
  makeWindowOf,
  filterWindowBooks,
  windowEntryLibraries,
  setMembersListing,
  setBranchSharing,
} from "./join";
import { isAppPath, isConsentPage, joinPage } from "./routes";

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

  it("include a client app's consent page", () => {
    expect(isAppPath("/client/approve")).toBe(true);
    expect(joinPage("/client/approve/")).toBe("client-approve");
    expect(isConsentPage("client-approve")).toBe(true);
    expect(isConsentPage("approve")).toBe(true);
    expect(isConsentPage("callback")).toBe(false);
    expect(isConsentPage(null)).toBe(false);
  });
});

describe("approve return", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("keeps either consent page once, and nothing else", () => {
    const store = new Map<string, string>();
    vi.stubGlobal("sessionStorage", {
      getItem: (key: string) => store.get(key) ?? null,
      setItem: (key: string, value: string) => void store.set(key, value),
      removeItem: (key: string) => void store.delete(key),
    });
    rememberApproveReturn("/client/approve?client_id=bookclerk-client");
    expect(takeApproveReturn()).toBe("/client/approve?client_id=bookclerk-client");
    expect(takeApproveReturn()).toBeNull();
    rememberApproveReturn("/join/approve?client_id=bookclerk-branch");
    expect(takeApproveReturn()).toBe("/join/approve?client_id=bookclerk-branch");
    rememberApproveReturn("/settings?x=1");
    expect(takeApproveReturn()).toBeNull();
  });
});

describe("answerClientConsent", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends the authorize request with the answer", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ redirect: "http://localhost:41923/client/callback" }), {
          status: 200,
        }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const search =
      "?client_id=bookclerk-client&response_type=code&scope=library%3Amember" +
      "&redirect_uri=http%3A%2F%2Flocalhost%3A41923%2Fclient%2Fcallback" +
      "&code_challenge=abc&code_challenge_method=S256&state=s1&client_name=Laptop";
    await answerClientConsent(search, true);
    expect(fetchMock.mock.calls[0][0]).toBe("/api/auth/client-consent");
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body).toMatchObject({
      client_id: "bookclerk-client",
      client_name: "Laptop",
      approve: true,
    });
    await answerClientConsent(search, false);
    const denied = JSON.parse(String(fetchMock.mock.calls[1][1].body));
    expect(denied.approve).toBe(false);
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
    expect(joinErrorMessage(new JoinApiError(410, "x", "join_expired"), "f")).toMatch(
      /Start again/,
    );
    expect(
      joinErrorMessage(new JoinApiError(409, "use this address", "origin_mismatch"), "f"),
    ).toBe("use this address");
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
    expect(body).not.toHaveProperty("bring_connections");
  });

  it("asks to bring connections only with a branch approval", async () => {
    const fetchMock = stubFetch();
    await answerBranchConsent(AUTHORIZE, true, undefined, true);
    await answerBranchConsent(AUTHORIZE, false, undefined, true);
    await answerBranchConsent(
      AUTHORIZE,
      true,
      { step_up: { method: "totp", code: "123456" } },
      true,
    );
    const bodies = fetchMock.mock.calls.map((call) => JSON.parse(String(call[1].body)));
    expect(bodies[0].bring_connections).toBe(true);
    expect(bodies[1]).not.toHaveProperty("bring_connections");
    expect(bodies[2]).not.toHaveProperty("bring_connections");
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

describe("setBranchSharing", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends only the work settings it is given", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ cluster: {} }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await setBranchSharing("c1", true, false);
    await setBranchSharing("c1", true, false, { workForMembers: true, keepMemberCopies: false });
    await setBranchSharing("c1", true, false, { keepMemberCopies: true });
    const bodies = fetchMock.mock.calls.map((call) => JSON.parse(String(call[1].body)));
    expect(bodies[0]).toEqual({ cluster_id: "c1", share_books: true, include_branches: false });
    expect(bodies[1]).toEqual({
      cluster_id: "c1",
      share_books: true,
      include_branches: false,
      work_for_members: true,
      keep_member_copies: false,
    });
    expect(bodies[2]).toEqual({
      cluster_id: "c1",
      share_books: true,
      include_branches: false,
      keep_member_copies: true,
    });
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

describe("listClerks", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("reads the Owner-wide clerk listing, not the owned-nodes one", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ node_id: "a", nodes: [] }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await expect(listClerks()).resolves.toEqual({ node_id: "a", nodes: [] });
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/clerks");
  });
});

describe("jobShareSummary", () => {
  it("words an equal share, a cap and no work", () => {
    expect(jobShareSummary(null, 2)).toBe("Equal share of this computer: 2 jobs at once now");
    expect(jobShareSummary(undefined)).toBe("Equal share of this computer");
    expect(jobShareSummary(3, 1)).toBe("Up to 3 jobs at once: 1 job at once now");
    expect(jobShareSummary(1)).toBe("Up to 1 job at once");
    expect(jobShareSummary(0, 0)).toBe("Runs no work for this library");
  });
});

describe("broughtConnectionsSummary", () => {
  const base = { offered: 3, added: 0, already_here: 0, taken: 0, not_kept: null } as const;

  it("says nothing when none came", () => {
    expect(broughtConnectionsSummary(undefined)).toBeNull();
    expect(broughtConnectionsSummary(null)).toBeNull();
    expect(broughtConnectionsSummary({ ...base, offered: 0 })).toBeNull();
  });

  it("counts what was added, already here, and taken", () => {
    expect(broughtConnectionsSummary({ ...base, added: 1, already_here: 1, taken: 1 })).toBe(
      "1 store connection added, 1 already here, 1 already connected by someone else here, left as they are.",
    );
    expect(broughtConnectionsSummary({ ...base, added: 2 })).toBe("2 store connections added.");
    expect(broughtConnectionsSummary({ ...base, added: 1, failed: 1 })).toBe(
      "1 store connection added, 1 couldn't be added; connect them in Accounts.",
    );
  });

  it("explains why none were kept", () => {
    expect(broughtConnectionsSummary({ ...base, not_kept: "not_allowed" })).toMatch(
      /administrator doesn't let you/,
    );
    expect(broughtConnectionsSummary({ ...base, not_kept: "no_owner" })).toMatch(/no single owner/);
    expect(broughtConnectionsSummary({ ...base, not_kept: "failed" })).toMatch(/couldn't be added/);
  });
});

describe("window view helpers", () => {
  const edition = (origin: string, store: string, formats: string[]) => ({
    cluster_id: origin,
    owner_origin: origin,
    store,
    marketplace: "us",
    product_id: "B0X",
    formats,
    title: "T",
  });

  it("names each library once, by host, in order", () => {
    const entry = {
      key: "work:1",
      editions: [
        edition("https://books.example.com", "audible", ["audio"]),
        edition("https://books.example.com", "libro", []),
        edition("https://other.example:8443", "audible", ["pdf", "audio"]),
        edition("not a url", "audible", []),
      ],
    };
    expect(windowEntryLibraries(entry)).toEqual([
      "books.example.com",
      "other.example:8443",
      "not a url",
    ]);
    expect(windowEntryFormats(entry)).toEqual(["audio", "pdf"]);
  });

  it("narrows to the entries matching a Library search", () => {
    const dune = {
      key: "work:1",
      editions: [
        { ...edition("https://a.example", "audible", []), title: "Dune", authors: "Frank Herbert" },
      ],
    };
    const emma = {
      key: "work:2",
      editions: [
        {
          ...edition("https://a.example", "audible", []),
          title: "Emma",
          series: "Classics",
          subtitle: null,
        },
      ],
    };
    const books = [dune, emma];
    expect(filterWindowBooks(books, "")).toEqual(books);
    expect(filterWindowBooks(books, "  ")).toEqual(books);
    expect(filterWindowBooks(books, "HERBERT")).toEqual([dune]);
    expect(filterWindowBooks(books, " classics ")).toEqual([emma]);
    expect(filterWindowBooks(books, "nothing")).toEqual([]);
  });
});

describe("connectionsAddedLine", () => {
  const report = {
    added: 1,
    already_here: 0,
    taken: 0,
    failed: 0,
    not_sent: 0,
    not_allowed: false,
  };

  it("says how many were added", () => {
    expect(connectionsAddedLine("books.example.com", report)).toBe("1 added to books.example.com.");
  });

  it("names every other outcome", () => {
    expect(
      connectionsAddedLine("books.example.com", {
        ...report,
        added: 0,
        already_here: 1,
        taken: 2,
        failed: 1,
        not_sent: 1,
      }),
    ).toBe(
      "0 added to books.example.com, 1 already there, 2 connected to someone else there, 2 not added.",
    );
  });

  it("says when the library doesn't allow it", () => {
    expect(connectionsAddedLine("books.example.com", { ...report, not_allowed: true })).toBe(
      "books.example.com doesn't let you connect store accounts there; nothing was added.",
    );
  });
});

describe("setMembersListing", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends the members setting with the link's current sharing settings", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ cluster: { cluster_id: "c1" } }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await setMembersListing(
      { cluster_id: "c1", share_books: false, include_branches: true },
      false,
    );
    await setMembersListing({ cluster_id: "c2" }, true);
    expect(fetchMock.mock.calls[0][0]).toBe("/api/config/join/sharing");
    const bodies = fetchMock.mock.calls.map((call) => JSON.parse(String(call[1].body)));
    expect(bodies[0]).toEqual({
      cluster_id: "c1",
      share_books: false,
      include_branches: true,
      list_members_books: false,
    });
    expect(bodies[1]).toEqual({
      cluster_id: "c2",
      share_books: true,
      include_branches: true,
      list_members_books: true,
    });
  });
});

describe("setLinkDownload", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("sends the download setting with the link's current sharing settings", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ cluster: { cluster_id: "c1" } }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    await setLinkDownload({ cluster_id: "c1", share_books: false, include_branches: true }, false);
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body).toEqual({
      cluster_id: "c1",
      share_books: false,
      include_branches: true,
      download: false,
    });
  });
});

describe("makeWindowOf", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("turns downloading off for the joined link", async () => {
    const fetchMock = vi.fn(
      async (_url: string, _init: RequestInit) =>
        new Response(JSON.stringify({ cluster: { cluster_id: "c1" } }), { status: 200 }),
    );
    vi.stubGlobal("fetch", fetchMock);
    expect(await makeWindowOf({ cluster_id: "c1", share_books: true, download: true })).toBe(true);
    const body = JSON.parse(String(fetchMock.mock.calls[0][1].body));
    expect(body).toEqual({
      cluster_id: "c1",
      share_books: true,
      include_branches: true,
      download: false,
    });
  });

  it("sends nothing when the link is already a window", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    expect(await makeWindowOf({ cluster_id: "c1", download: false })).toBe(true);
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("answers false when the setting could not be stored", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(JSON.stringify({ error: "nope" }), { status: 500 })),
    );
    expect(await makeWindowOf({ cluster_id: "c1" })).toBe(false);
  });
});
