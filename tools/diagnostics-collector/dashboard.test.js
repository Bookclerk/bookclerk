// Dashboard tests: sign-in, sessions, pages, index and issue sync, run under
// Node with a node:sqlite stand-in for D1 and a scripted GitHub.

import assert from "node:assert/strict";
import { generateKeyPairSync, verify as rsaVerify } from "node:crypto";
import { afterEach, beforeEach, describe, it } from "node:test";
import { OAUTH_COOKIE, pkceChallenge } from "./dashboard/auth.js";
import { afterFixCounts, all, indexReport, first, run, topSignatures } from "./dashboard/db.js";
import { appJwt, clearTokenCache, githubConfig, INSTALLATION_PERMISSIONS, pkcs8FromPem } from "./dashboard/github.js";
import { escapeHtml, fmtTime, html, isoTime } from "./dashboard/html.js";
import { buckets } from "./dashboard/pages.js";
import { parseIssueReportIds, reportIdOf } from "./dashboard/report-ids.js";
import { dashboardFetch } from "./dashboard/routes.js";
import { SESSION_COOKIE, createSession } from "./dashboard/session.js";
import { normalizeMessage, summarizeReport } from "./dashboard/signature.js";
import { storeIssue, syncIssues } from "./dashboard/sync.js";
import { createD1 } from "./test-support/d1.js";

const ORIGIN = "https://diagnostics.example.test";
const NOW = Date.UTC(2026, 9, 23, 14, 0, 0);
const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const PKCS1 = privateKey.export({ type: "pkcs1", format: "pem" });
const PKCS8 = privateKey.export({ type: "pkcs8", format: "pem" });
const PUBLIC = privateKey;

/** Worker env with the dashboard on. */
function makeEnv(overrides = {}) {
  return {
    DASHBOARD_DB: createD1(),
    GITHUB_REPOSITORY: "Bookclerk/bookclerk",
    GITHUB_APP_CLIENT_ID: "Iv23liTestClient",
    GITHUB_APP_CLIENT_SECRET: "test-client-secret",
    GITHUB_APP_PRIVATE_KEY: PKCS1,
    ...overrides,
  };
}

/** Execution context that records waitUntil promises. */
function makeCtx() {
  const pending = [];
  return { pending, waitUntil: (p) => pending.push(p) };
}

/** B2 stand-in serving stored reports by key. */
function makeB2(objects = {}) {
  return {
    bucketId: "bucket",
    authorize: async () => ({ apiUrl: "https://b2.test", authorizationToken: "b2tok", downloadUrl: "https://b2dl.test" }),
    downloadById: async (_auth, id) => objects[id],
  };
}

/** Normalizes the path the way worker.js does and calls the dashboard. */
function call(env, req, b2 = makeB2(), ctx = makeCtx(), now = NOW) {
  const path = new URL(req.url).pathname.replace(/\/+$/, "") || "/";
  return dashboardFetch(req, env, ctx, path, b2, now);
}

/** Scripted GitHub: routes matched by method + URL prefix, calls recorded. */
function scriptGitHub(routes) {
  const calls = [];
  const real = globalThis.fetch;
  globalThis.fetch = async (input, init = {}) => {
    const url = String(input);
    const method = init.method || "GET";
    const body = init.body ? JSON.parse(init.body) : null;
    calls.push({ method, url, body, headers: init.headers });
    for (const [key, handler] of routes) {
      const [m, prefix] = key.split(" ");
      if (m === method && url.startsWith(prefix)) return handler({ url, body, headers: init.headers });
    }
    if (url === "https://b2.test/b2api/v2/b2_list_file_names") {
      return Response.json({ files: [{ action: "upload", fileName: body.prefix, fileId: `id:${body.prefix}`, uploadTimestamp: NOW }] });
    }
    throw new Error(`unscripted fetch ${method} ${url}`);
  };
  return { calls, restore: () => (globalThis.fetch = real) };
}

/** GitHub routes for a successful App token and the given membership. */
function appRoutes(membership) {
  return [
    ["GET https://api.github.com/repos/Bookclerk/bookclerk/installation", () => Response.json({ id: 42 })],
    [
      "POST https://api.github.com/app/installations/42/access_tokens",
      () => Response.json({ token: "ghs_installation", expires_at: new Date(NOW + 3_600_000).toISOString() }),
    ],
    [
      "GET https://api.github.com/orgs/Bookclerk/memberships/",
      () => (membership ? Response.json(membership) : new Response("{}", { status: 404 })),
    ],
  ];
}

/** Signs a member in directly and returns the Cookie header. */
async function signedIn(env, role = "member") {
  const setCookie = await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role }, NOW);
  return setCookie.split(";")[0];
}

/** A stored report as /submit writes it. */
function stored(id, overrides = {}) {
  return {
    schema_version: 1,
    report_id: id,
    received_at_unix_ms: NOW - 3_600_000,
    client_ip_hash: "5e0c00000000000000000000000019ab",
    payload: {
      trigger: "error_burst",
      version: "0.9.0-rc.2",
      os: "linux",
      arch: "x86_64",
      distro: "Debian 13",
      rustc_release: "1.91.0",
      rustc_channel: "stable",
      events: [
        { ts_unix_ms: NOW - 3_600_000, level: "INFO", target: "bookclerk::jobs", message: "queued", fields: [] },
        {
          ts_unix_ms: NOW - 3_599_000,
          level: "ERROR",
          target: "libro::download",
          message: "range 8388608 timed out after 15000 ms at /home/u/x <script>alert(1)</script>",
          fields: [["host", "[REDACTED]"]],
        },
      ],
      ...overrides,
    },
  };
}

const ID1 = "7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4";
const ID2 = "d2b80f13-0000-4000-8000-000000000002";

/** Indexes a stored report. */
async function seed(env, id, receivedAt = NOW - 3_600_000, overrides = {}) {
  const s = stored(id, overrides);
  s.received_at_unix_ms = receivedAt;
  await indexReport(
    env.DASHBOARD_DB,
    { reportId: id, objectKey: `diagnostics/0.9.0-rc.2/${id}.json`, receivedAt, summary: await summarizeReport(s) },
    Infinity,
  );
  return s;
}

let gh = null;
beforeEach(() => clearTokenCache());
afterEach(() => {
  gh?.restore();
  gh = null;
});

describe("html", () => {
  it("escapes interpolations, including quotes", () => {
    assert.equal(escapeHtml(`<a href="x">'&`), "&lt;a href=&quot;x&quot;&gt;&#39;&amp;");
    assert.equal(String(html`<p title="${'"><script>'}">${"<b>"}</p>`), '<p title="&quot;&gt;&lt;script&gt;">&lt;b&gt;</p>');
  });
});

describe("signature", () => {
  it("normalizes volatile parts of a message", () => {
    assert.equal(
      normalizeMessage(`range 8388608-16777215 of "a.mp3" failed for 7c1e4a90-2b6f-4d0e-9a51-3f8e2d61b0c4 at /tmp/x/y and https://h.test/p?q=1`),
      "range <n>-<n> of <str> failed for <id> at <path> and <url>",
    );
  });

  it("groups the same failure from different machines", async () => {
    const a = await summarizeReport(stored(ID1));
    const b = await summarizeReport(
      stored(ID2, {
        events: [{ level: "error", target: "libro::download", message: "range 99 timed out after 20000 ms at /Users/v/z <script>alert(1)</script>" }],
      }),
    );
    assert.equal(a.signature, b.signature);
    assert.match(a.signature, /^[0-9a-f]{16}$/);
    assert.equal(a.errors, 1);
    assert.equal(a.events, 2);
    assert.equal(a.installHash, "5e0c00000000000000000000000019ab");
  });

  it("never throws on hostile shapes", async () => {
    for (const s of [null, [], 1, "x", { payload: [] }, { payload: { events: [null, 1, { level: 5 }] } }]) {
      const out = await summarizeReport(s);
      assert.equal(out.signature, null);
    }
  });
});

describe("report ids", () => {
  it("reads the triage and fallback sections", () => {
    const triage = `<!-- bookclerk-diagnostics: workers-ai triage -->\nSummary\n\n### Report IDs\n- \`${ID1}\`\n- \`diagnostics/0.9.0/${ID2}.json\`\n- \`unknown\`\n`;
    assert.deepEqual(parseIssueReportIds(triage), { kind: "triage", ids: [ID1, ID2] });
    const fallback = `<!-- bookclerk-diagnostics: triage fallback -->\n## Diagnostics batch\n\n### Report IDs\n- \`${ID2}\`\n\nAutomated triage did not cover these.`;
    assert.deepEqual(parseIssueReportIds(fallback), { kind: "fallback", ids: [ID2] });
  });

  it("uses only the last section and stops at the first other line", () => {
    const body = `### Report IDs\n- \`${ID1}\`\n\n### Report IDs\n- \`${ID2}\`\ntext\n- \`${ID1}\``;
    assert.deepEqual(parseIssueReportIds(body).ids, [ID2]);
  });

  it("rejects non-report ids", () => {
    assert.equal(reportIdOf("../../etc/passwd"), null);
    assert.equal(reportIdOf("diagnostics/x/../y.json"), null);
    assert.equal(reportIdOf(ID1.toUpperCase()), ID1);
  });
});

describe("app key", () => {
  it("converts PKCS#1 to the same key as PKCS#8 and signs a verifiable JWT", async () => {
    const fromPkcs1 = pkcs8FromPem(PKCS1);
    const fromPkcs8 = pkcs8FromPem(PKCS8);
    assert.deepEqual(Buffer.from(fromPkcs1), Buffer.from(fromPkcs8));
    const cfg = githubConfig(makeEnv());
    const jwt = await appJwt(cfg, NOW);
    const [h, p, s] = jwt.split(".");
    const claims = JSON.parse(Buffer.from(p, "base64url"));
    assert.equal(claims.iss, "Iv23liTestClient");
    assert.ok(claims.exp - claims.iat <= 600);
    assert.ok(rsaVerify("sha256", Buffer.from(`${h}.${p}`), PUBLIC, Buffer.from(s, "base64url")));
  });

  it("leaves the dashboard off without every setting", () => {
    for (const key of ["GITHUB_APP_CLIENT_ID", "GITHUB_APP_CLIENT_SECRET", "GITHUB_APP_PRIVATE_KEY", "GITHUB_REPOSITORY"]) {
      assert.equal(githubConfig(makeEnv({ [key]: "" })), null, key);
    }
  });
});

describe("routing", () => {
  it("returns null for every path when the dashboard is off", async () => {
    const env = makeEnv({ GITHUB_APP_PRIVATE_KEY: undefined });
    for (const p of ["/", "/dash", "/auth/github", `/reports/${ID1}`]) {
      assert.equal(await call(env, new Request(ORIGIN + p)), null, p);
    }
    assert.equal(await call(makeEnv({ DASHBOARD_DB: undefined }), new Request(`${ORIGIN}/`)), null);
  });

  it("leaves collector paths to worker.js", async () => {
    const env = makeEnv();
    for (const p of ["/submit", "/report", "/triage", "/health"]) {
      assert.equal(await call(env, new Request(ORIGIN + p)), null, p);
    }
  });

  it("shows sign-in to visitors and sends them there from member pages", async () => {
    const env = makeEnv();
    const home = await call(env, new Request(`${ORIGIN}/`));
    assert.equal(home.status, 200);
    assert.match(home.headers.get("content-security-policy"), /default-src 'none'/);
    assert.equal(home.headers.get("cache-control"), "no-store");
    assert.match(await home.text(), /Sign in with GitHub/);
    for (const p of ["/dash", "/reports", "/issues", `/reports/${ID1}`]) {
      const res = await call(env, new Request(ORIGIN + p));
      assert.equal(res.status, 303, p);
      assert.equal(res.headers.get("location"), "/");
    }
  });
});

describe("sign-in", () => {
  it("starts with state and PKCE in a __Host- cookie", async () => {
    const res = await call(makeEnv(), new Request(`${ORIGIN}/auth/github`));
    assert.equal(res.status, 303);
    const loc = new URL(res.headers.get("location"));
    assert.equal(loc.origin + loc.pathname, "https://github.com/login/oauth/authorize");
    assert.equal(loc.searchParams.get("redirect_uri"), `${ORIGIN}/auth/callback`);
    assert.equal(loc.searchParams.get("code_challenge_method"), "S256");
    const cookie = res.headers.get("set-cookie");
    assert.match(cookie, /^__Host-bc_oauth=[A-Za-z0-9_-]{43}\.[A-Za-z0-9_-]{43}; Path=\/; Secure; HttpOnly; SameSite=Lax; Max-Age=600$/);
    const [state, verifier] = cookie.split(";")[0].split("=")[1].split(".");
    assert.equal(loc.searchParams.get("state"), state);
    assert.equal(loc.searchParams.get("code_challenge"), await pkceChallenge(verifier));
  });

  /** Runs a callback with a valid state cookie. */
  async function callback(env, ctx) {
    const state = "s".repeat(43);
    const verifier = "v".repeat(43);
    return call(
      env,
      new Request(`${ORIGIN}/auth/callback?code=abc123&state=${state}`, {
        headers: { cookie: `${OAUTH_COOKIE}=${state}.${verifier}` },
      }),
      makeB2(),
      ctx,
    );
  }

  /** GitHub routes for code exchange, user lookup and token revocation. */
  const userRoutes = [
    ["POST https://github.com/login/oauth/access_token", () => Response.json({ access_token: "ghu_user" })],
    ["GET https://api.github.com/user", () => Response.json({ id: 7, login: "octo" })],
    ["DELETE https://api.github.com/applications/Iv23liTestClient/token", () => new Response(null, { status: 204 })],
  ];

  it("names GitHub's reason when it refuses the code and trims the client secret", async () => {
    gh = scriptGitHub([
      [
        "POST https://github.com/login/oauth/access_token",
        ({ body }) => {
          assert.equal(body.client_secret, "test-client-secret", "a pasted trailing newline is dropped");
          return Response.json({ error: "incorrect_client_credentials", error_description: "The client_id and/or client_secret passed are incorrect." });
        },
      ],
    ]);
    const res = await callback(makeEnv({ GITHUB_APP_CLIENT_SECRET: "test-client-secret\n" }), makeCtx());
    assert.equal(res.status, 400);
    const text = await res.text();
    assert.match(text, /code exchange/);
    assert.match(text, /<code>incorrect_client_credentials<\/code>/);
    assert.doesNotMatch(text, /client_secret passed/, "GitHub's free text is not echoed");
  });

  it("names the HTTP status when GitHub refuses the account lookup", async () => {
    gh = scriptGitHub([
      userRoutes[0],
      ["GET https://api.github.com/user", () => new Response("{}", { status: 403 })],
      userRoutes[2],
    ]);
    const ctx = makeCtx();
    const res = await callback(makeEnv(), ctx);
    await Promise.all(ctx.pending);
    assert.equal(res.status, 400);
    assert.match(await res.text(), /account lookup[\s\S]*<code>HTTP 403<\/code>/);
    assert.ok(gh.calls.some((c) => c.method === "DELETE"), "the user token is still revoked");
  });

  it("signs in an active member with read-only tokens and revokes the user token", async () => {
    gh = scriptGitHub([...userRoutes, ...appRoutes({ state: "active", role: "admin", user: { id: 7 } })]);
    const env = makeEnv();
    const ctx = makeCtx();
    const res = await callback(env, ctx);
    await Promise.all(ctx.pending);
    assert.equal(res.status, 200);
    assert.match(await res.text(), /url=\/dash/);
    const cookies = res.headers.getSetCookie();
    assert.ok(cookies.some((c) => c.startsWith(`${OAUTH_COOKIE}=;`)));
    const session = cookies.find((c) => c.startsWith(`${SESSION_COOKIE}=`));
    assert.match(session, /HttpOnly; SameSite=Strict; Max-Age=28800$/);

    const exchange = gh.calls.find((c) => c.url.startsWith("https://github.com/login/oauth/access_token"));
    assert.equal(exchange.body.code_verifier, "v".repeat(43));
    const mint = gh.calls.find((c) => c.url.endsWith("/access_tokens"));
    assert.deepEqual(mint.body, { repositories: ["bookclerk"], permissions: INSTALLATION_PERMISSIONS });
    assert.ok(Object.values(mint.body.permissions).every((v) => v === "read"));
    const revoke = gh.calls.find((c) => c.method === "DELETE");
    assert.deepEqual(revoke.body, { access_token: "ghu_user" });
    const member = gh.calls.find((c) => c.url.includes("/memberships/"));
    assert.equal(member.headers.Authorization, "Bearer ghs_installation");

    const row = await first(env.DASHBOARD_DB, "SELECT login, role, id_hash FROM sessions");
    assert.deepEqual({ login: row.login, role: row.role }, { login: "octo", role: "admin" });
    assert.ok(!session.includes(row.id_hash), "cookie is not the stored hash");
    const log = await all(env.DASHBOARD_DB, "SELECT action FROM access_log");
    assert.deepEqual(log.map((r) => r.action), ["sign-in"]);

    const dash = await call(env, new Request(`${ORIGIN}/dash`, { headers: { cookie: session.split(";")[0] } }));
    assert.equal(dash.status, 200);
    assert.match(await dash.text(), /Owner tools/);
  });

  it("refuses non-members, pending members and mismatched ids", async () => {
    for (const m of [null, { state: "pending", role: "member", user: { id: 7 } }, { state: "active", role: "member", user: { id: 8 } }]) {
      clearTokenCache();
      gh = scriptGitHub([...userRoutes, ...appRoutes(m)]);
      const env = makeEnv();
      const res = await callback(env, makeCtx());
      assert.equal(res.status, 403);
      assert.match(await res.text(), /for organization members/);
      assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM sessions")).length, 0);
      gh.restore();
    }
    gh = null;
  });

  it("reports a GitHub failure instead of calling it non-membership", async () => {
    gh = scriptGitHub([
      ...userRoutes,
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/installation", () => new Response("no", { status: 500 })],
    ]);
    const res = await callback(makeEnv(), makeCtx());
    assert.equal(res.status, 502);
  });

  it("rejects a callback whose state does not match the cookie", async () => {
    gh = scriptGitHub([]);
    const res = await call(
      makeEnv(),
      new Request(`${ORIGIN}/auth/callback?code=abc&state=${"a".repeat(43)}`, {
        headers: { cookie: `${OAUTH_COOKIE}=${"b".repeat(43)}.${"v".repeat(43)}` },
      }),
    );
    assert.equal(res.status, 400);
    assert.equal(gh.calls.length, 0);
  });
});

describe("sessions", () => {
  it("expires after 8 hours", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const ok = await call(env, new Request(`${ORIGIN}/issues`, { headers: { cookie } }), makeB2(), makeCtx(), NOW + 7 * 3_600_000);
    assert.equal(ok.status, 200);
    const late = await call(env, new Request(`${ORIGIN}/issues`, { headers: { cookie } }), makeB2(), makeCtx(), NOW + 8 * 3_600_000);
    assert.equal(late.status, 303);
  });

  it("logs out only from this origin", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const foreign = await call(env, new Request(`${ORIGIN}/auth/logout`, { method: "POST", headers: { cookie, origin: "https://evil.test" } }));
    assert.equal(foreign.status, 403);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM sessions")).length, 1);
    const own = await call(env, new Request(`${ORIGIN}/auth/logout`, { method: "POST", headers: { cookie, origin: ORIGIN } }));
    assert.equal(own.status, 303);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM sessions")).length, 0);
  });

  it("keeps owner actions to owners", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env, "member");
    const res = await call(env, new Request(`${ORIGIN}/admin/sign-out-all`, { method: "POST", headers: { cookie, origin: ORIGIN } }));
    assert.equal(res.status, 403);
    const ownerCookie = await signedIn(env, "admin");
    const cross = await call(env, new Request(`${ORIGIN}/admin/sign-out-all`, { method: "POST", headers: { cookie: ownerCookie } }));
    assert.equal(cross.status, 403);
    const done = await call(env, new Request(`${ORIGIN}/admin/sign-out-all`, { method: "POST", headers: { cookie: ownerCookie, origin: ORIGIN } }));
    assert.equal(done.status, 303);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM sessions")).length, 0);
  });
});

describe("pages", () => {
  it("renders every page with report text escaped and logs report views", async () => {
    gh = scriptGitHub([]);
    const env = makeEnv();
    const s1 = await seed(env, ID1);
    await seed(env, ID2, NOW - 2 * 86_400_000);
    await storeIssue(env.DASHBOARD_DB, {
      number: 412,
      title: "[diagnostics] Libro <b>stalls</b>",
      state: "open",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID2}\`\n`,
      created_at: new Date(NOW - 3 * 86_400_000).toISOString(),
      updated_at: new Date(NOW - 86_400_000).toISOString(),
    });
    const cookie = await signedIn(env);
    const b2 = makeB2({ [`id:diagnostics/0.9.0-rc.2/${ID1}.json`]: JSON.stringify(s1) });
    for (const p of ["/dash", "/dash?range=24h&version=0.9.0-rc.2", "/reports", "/reports?filed=no&range=90d", "/issues?state=all", `/reports/${ID1}`]) {
      const res = await call(env, new Request(ORIGIN + p, { headers: { cookie } }), b2);
      assert.equal(res.status, 200, p);
      const body = await res.text();
      assert.ok(!body.includes("<script>alert"), `${p} escapes report text`);
      assert.ok(!body.includes("<b>stalls"), `${p} escapes issue titles`);
    }
    const report = await (await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers: { cookie } }), b2)).text();
    assert.match(report, /&lt;script&gt;alert\(1\)&lt;\/script&gt;/);
    assert.match(report, /\[REDACTED\]/);
    const issues = await (await call(env, new Request(`${ORIGIN}/issues`, { headers: { cookie } }), b2)).text();
    assert.match(issues, /https:\/\/github.com\/Bookclerk\/bookclerk\/issues\/412/);
    const log = await all(env.DASHBOARD_DB, "SELECT action, report_id FROM access_log WHERE action = 'view-report'");
    assert.equal(log.length, 2);
    assert.equal(log[0].report_id, ID1);
  });

  it("says when a report is not indexed", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const res = await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers: { cookie } }));
    assert.equal(res.status, 404);
  });
});

describe("index", () => {
  it("stops at the daily cap and ignores duplicates", async () => {
    const db = createD1();
    const summary = await summarizeReport(stored(ID1));
    const row = (id) => ({ reportId: id, objectKey: `diagnostics/x/${id}.json`, receivedAt: NOW, summary });
    assert.equal(await indexReport(db, row(ID1), 1), "indexed");
    assert.equal(await indexReport(db, row(ID1), 2), "indexed");
    assert.equal((await first(db, "SELECT rows FROM index_days")).rows, 1);
    assert.equal(await indexReport(db, row(ID2), 1), "capped");
    assert.equal((await all(db, "SELECT * FROM reports")).length, 1);
  });
});

describe("issue sync", () => {
  it("links reports to issues and counts reports after a merged fix", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000);
    await seed(env, ID2, NOW - 3_600_000);
    const issue = {
      number: 412,
      title: "[diagnostics] Libro stalls",
      state: "open",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n`,
      created_at: new Date(NOW - 4 * 86_400_000).toISOString(),
      updated_at: new Date(NOW - 2 * 86_400_000).toISOString(),
      closed_at: null,
    };
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([issue, { number: 500, pull_request: {} }])],
      [
        "POST https://api.github.com/graphql",
        ({ body }) => {
          assert.match(body.query, /i0: issue\(number: 412\)/);
          return Response.json({
            data: {
              repository: {
                i0: {
                  closedByPullRequestsReferences: {
                    nodes: [{ number: 418, title: "Retry", state: "MERGED", mergedAt: new Date(NOW - 86_400_000).toISOString(), repository: { nameWithOwner: "Bookclerk/bookclerk" } }],
                  },
                  timelineItems: {
                    nodes: [
                      { source: { number: 9, title: "Elsewhere", state: "OPEN", mergedAt: null, repository: { nameWithOwner: "other/repo" } } },
                      {},
                    ],
                  },
                },
              },
            },
          });
        },
      ],
    ]);
    const res = await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(res, { issues: 1, refreshed: 1 });
    const links = await all(env.DASHBOARD_DB, "SELECT * FROM report_issue");
    assert.deepEqual(links.map((l) => [l.report_id, l.issue_number]), [[ID1, 412]]);
    const prs = await all(env.DASHBOARD_DB, "SELECT pr_number, state FROM issue_pr");
    assert.deepEqual(prs.map((p) => [p.pr_number, p.state]), [[418, "merged"]]);
    const after = await afterFixCounts(env.DASHBOARD_DB);
    assert.equal(after.get(412).after, 1, "the newer report shares the signature");
    const list = gh.calls.find((c) => c.url.includes("/issues?"));
    assert.match(list.url, /labels=diagnostics/);
  });
});

describe("pull request refresh", () => {
  /** GraphQL route answering every requested issue with no pull requests. */
  function emptyGraphql(seen) {
    return [
      "POST https://api.github.com/graphql",
      ({ body }) => {
        const repository = {};
        for (const [, alias, n] of body.query.matchAll(/(i\d+): issue\(number: (\d+)\)/g)) {
          seen.push(Number(n));
          repository[alias] = { closedByPullRequestsReferences: { nodes: [] }, timelineItems: { nodes: [] } };
        }
        return Response.json({ data: { repository } });
      },
    ];
  }

  /** Inserts closed issues queued for a refresh. */
  async function queuedIssues(db, count) {
    for (let n = 1; n <= count; n++) {
      await run(db, "INSERT INTO issues (number, title, state, kind, created_at, updated_at) VALUES (?, 't', 'closed', 'triage', 0, 0)", n);
    }
  }

  const noNewIssues = ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([])];

  it("keeps stored links when GraphQL answers with errors", async () => {
    const env = makeEnv();
    const db = env.DASHBOARD_DB;
    await queuedIssues(db, 1);
    await run(db, "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (1, 9, 'fix', 'merged', ?)", NOW);
    gh = scriptGitHub([
      ...appRoutes(null),
      noNewIssues,
      ["POST https://api.github.com/graphql", () => Response.json({ errors: [{ message: "rate limited" }], data: null })],
    ]);
    await assert.rejects(syncIssues(env, githubConfig(env), NOW), /graphql returned errors/);
    assert.equal((await all(db, "SELECT * FROM issue_pr")).length, 1);
    assert.equal((await first(db, "SELECT prs_synced_at FROM issues")).prs_synced_at, null, "still queued");
  });

  it("keeps links for an issue GitHub returned no data for", async () => {
    const env = makeEnv();
    const db = env.DASHBOARD_DB;
    await queuedIssues(db, 1);
    await run(db, "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (1, 9, 'fix', 'merged', ?)", NOW);
    gh = scriptGitHub([
      ...appRoutes(null),
      noNewIssues,
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: { i0: null } } })],
    ]);
    const res = await syncIssues(env, githubConfig(env), NOW);
    assert.equal(res.refreshed, 0);
    assert.equal((await all(db, "SELECT * FROM issue_pr")).length, 1);
    assert.equal((await first(db, "SELECT prs_synced_at FROM issues")).prs_synced_at, NOW, "moved to the back of the queue");
  });

  it("carries issues past the per-run limit to the next run", async () => {
    const env = makeEnv();
    const db = env.DASHBOARD_DB;
    await queuedIssues(db, 105);
    const seen = [];
    gh = scriptGitHub([...appRoutes(null), noNewIssues, emptyGraphql(seen)]);
    assert.equal((await syncIssues(env, githubConfig(env), NOW)).refreshed, 100);
    assert.equal((await first(db, "SELECT COUNT(*) AS n FROM issues WHERE prs_synced_at IS NULL")).n, 5);
    seen.length = 0;
    await syncIssues(env, githubConfig(env), NOW + 1);
    assert.deepEqual(seen.slice(0, 5), [101, 102, 103, 104, 105], "the carried-over issues go first");
    assert.equal((await first(db, "SELECT COUNT(*) AS n FROM issues WHERE prs_synced_at IS NULL")).n, 0);
  });

  it("drops an issue that lost the diagnostics label", async () => {
    const env = makeEnv();
    const db = env.DASHBOARD_DB;
    await queuedIssues(db, 1);
    await run(db, "INSERT INTO report_issue (report_id, issue_number) VALUES (?, 1)", ID1);
    await run(db, "INSERT INTO issue_pr (issue_number, pr_number, title, state, merged_at) VALUES (1, 9, 'fix', 'merged', ?)", NOW);
    let query = "";
    gh = scriptGitHub([
      ...appRoutes(null),
      noNewIssues,
      [
        "POST https://api.github.com/graphql",
        ({ body }) => {
          query = body.query;
          return Response.json({
            data: { repository: { i0: { labels: { nodes: [{ name: "bug" }], pageInfo: { hasNextPage: false } }, closedByPullRequestsReferences: { nodes: [] }, timelineItems: { nodes: [] } } } },
          });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.match(query, /timelineItems\(last: 50/);
    for (const table of ["issues", "report_issue", "issue_pr"]) {
      assert.equal((await all(db, `SELECT * FROM ${table}`)).length, 0, table);
    }
  });
});

describe("untrusted times and pages", () => {
  it("charts the partial oldest bucket the totals count", () => {
    const hour = 3_600_000;
    const now = Date.UTC(2026, 9, 24, 12, 30);
    const oldest = Math.floor((now - 24 * hour) / hour);
    const out = buckets(now, { ms: 24 * hour, bucket: hour }, new Map([[oldest, { panic: 1 }]]));
    assert.equal(out.length, 25);
    assert.deepEqual(out[0], { label: "12:00", values: { panic: 1 } });
    assert.equal(buckets(Date.UTC(2026, 9, 24, 12), { ms: 24 * hour, bucket: hour }, new Map()).length, 25);
  });

  it("formats out-of-range times as empty instead of throwing", () => {
    assert.equal(isoTime(1e20), "");
    assert.equal(isoTime(Number.NaN), "");
    assert.equal(isoTime("5"), "");
    assert.equal(fmtTime(-1e16), "");
    assert.equal(fmtTime(0), "1970-01-01 00:00");
  });

  it("pages past a full page of pull requests and moves the cursor", async () => {
    const env = makeEnv();
    const t = (m) => new Date(NOW - 86_400_000 + m * 60_000).toISOString();
    const prs = Array.from({ length: 100 }, (_, i) => ({ number: 1000 + i, pull_request: {}, updated_at: t(i) }));
    const issue = { number: 7, title: "x", state: "closed", body: "", created_at: t(0), updated_at: t(200), closed_at: t(200) };
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", ({ url }) => Response.json(url.includes("page=1&") || url.endsWith("page=1") ? prs : [issue])],
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: { i0: { closedByPullRequestsReferences: { nodes: [] }, timelineItems: { nodes: [] } } } } })],
    ]);
    const res = await syncIssues(env, githubConfig(env), NOW);
    assert.equal(res.issues, 1);
    const since = (await first(env.DASHBOARD_DB, "SELECT value FROM sync_state WHERE key = 'issues_since'")).value;
    assert.equal(since, new Date(Date.parse(t(200)) - 1000).toISOString(), "one second back, so ties are reread");
  });

  it("keeps an unchanged issue's refresh time when it is read again", async () => {
    const db = createD1();
    const issue = { number: 9, title: "x", state: "open", body: "", created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-02T00:00:00Z" };
    await storeIssue(db, issue);
    await run(db, "UPDATE issues SET prs_synced_at = 5 WHERE number = 9");
    await storeIssue(db, issue);
    assert.equal((await first(db, "SELECT prs_synced_at FROM issues")).prs_synced_at, 5);
    await storeIssue(db, { ...issue, updated_at: "2026-10-03T00:00:00Z" });
    assert.equal((await first(db, "SELECT prs_synced_at FROM issues")).prs_synced_at, null);
  });

  it("ignores an install hash that is not a string", async () => {
    for (const client_ip_hash of [["abc"], { toString: null }, 12]) {
      assert.equal((await summarizeReport({ ...stored(ID1), client_ip_hash })).installHash, "");
    }
  });
});

describe("issue paging and filtered trends", () => {
  it("works through more tied issue updates than one run reads", async () => {
    const env = makeEnv();
    const at = new Date(NOW - 86_400_000).toISOString();
    const issues = Array.from({ length: 600 }, (_, i) => ({ number: i + 1, title: "x", state: "open", body: "", created_at: at, updated_at: at }));
    gh = scriptGitHub([
      ...appRoutes(null),
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/issues?",
        ({ url }) => {
          const page = Number(new URL(url).searchParams.get("page"));
          return Response.json(issues.slice((page - 1) * 100, page * 100));
        },
      ],
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: {} } })],
    ]);
    const page = async () => (await first(env.DASHBOARD_DB, "SELECT value FROM sync_state WHERE key = 'issues_page'"))?.value ?? null;
    await syncIssues(env, githubConfig(env), NOW);
    assert.equal(await page(), "4", "the tied pass continues next run from the last page read");
    await syncIssues(env, githubConfig(env), NOW);
    assert.equal((await first(env.DASHBOARD_DB, "SELECT COUNT(*) AS n FROM issues")).n, 600);
    assert.equal(await page(), null, "back to the first page once drained");
    const since = (await first(env.DASHBOARD_DB, "SELECT value FROM sync_state WHERE key = 'issues_since'")).value;
    assert.equal(since, new Date(Date.parse(at) - 1000).toISOString());
  });

  it("counts a signature's daily trend inside the version filter", async () => {
    const db = createD1();
    const ids = ["00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002"];
    for (const [i, version] of ["1.0.0", "2.0.0"].entries()) {
      const summary = await summarizeReport(stored(ids[i], { version }));
      await indexReport(db, { reportId: ids[i], objectKey: `diagnostics/${version}/${ids[i]}.json`, receivedAt: NOW - 1000, summary });
    }
    const [sig] = await topSignatures(db, NOW - 86_400_000, { version: "1.0.0" }, 5);
    assert.equal(sig.n, 1);
    assert.equal([...sig.days.values()].reduce((a, b) => a + b, 0), 1);
  });
});

describe("index cap under concurrency", () => {
  it("indexes at most the cap when requests race", async () => {
    const db = createD1();
    const summary = await summarizeReport(stored(ID1));
    const ids = Array.from({ length: 10 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`);
    const results = await Promise.all(
      ids.map((id) => indexReport(db, { reportId: id, objectKey: `diagnostics/x/${id}.json`, receivedAt: NOW, summary }, 1)),
    );
    assert.equal(results.filter((r) => r === "indexed").length, 1);
    assert.equal((await all(db, "SELECT * FROM reports")).length, 1);
    assert.equal((await first(db, "SELECT rows FROM index_days")).rows, 1);
  });
});

/**
 * A D1 handle that fails any call past `limit`, counting each batch as one
 * call, the way Workers Free limits D1 queries per invocation. Independent of
 * dashboard/budget.js, so it checks that code rather than mirroring it.
 */
function limitedD1(d1, limit) {
  const handle = { calls: 0 };
  const charge = () => {
    handle.calls += 1;
    if (handle.calls > limit) throw new Error(`D1 call ${handle.calls} exceeds ${limit}`);
  };
  const wrap = (stmt) => ({
    stmt,
    bind: (...p) => wrap(stmt.bind(...p)),
    all: () => (charge(), stmt.all()),
    first: () => (charge(), stmt.first()),
    run: () => (charge(), stmt.run()),
  });
  handle.prepare = (sql) => wrap(d1.prepare(sql));
  handle.batch = (stmts) => (charge(), d1.batch(stmts.map((s) => s.stmt)));
  return handle;
}

describe("per-invocation D1 limit", () => {
  it("works through an issue and pull request backlog in 50-call runs", async () => {
    const d1 = createD1();
    const env = makeEnv();
    const t = (i) => new Date(NOW - 86_400_000 + i * 1000).toISOString();
    const issues = Array.from({ length: 1150 }, (_, i) => ({
      number: i + 1,
      title: "x",
      state: "open",
      body: `### Report IDs\n- \`00000000-0000-4000-8000-${String(i).padStart(12, "0")}\`\n`,
      created_at: t(i),
      updated_at: t(i),
    }));
    let fetches = 0;
    gh = scriptGitHub([
      ...appRoutes(null),
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/issues?",
        ({ url }) => {
          fetches += 1;
          const q = new URL(url).searchParams;
          const since = q.get("since") ?? "";
          const page = Number(q.get("page"));
          return Response.json(issues.filter((i) => i.updated_at >= since).slice((page - 1) * 100, page * 100));
        },
      ],
      [
        "POST https://api.github.com/graphql",
        ({ body }) => {
          fetches += 1;
          const repository = {};
          for (const [, alias, n] of body.query.matchAll(/(i\d+): issue\(number: (\d+)\)/g)) {
            repository[alias] = {
              labels: { nodes: [{ name: "diagnostics" }], pageInfo: { hasNextPage: false } },
              closedByPullRequestsReferences: { nodes: [{ number: 5000 + Number(n), title: "fix", state: "OPEN", mergedAt: null, repository: { nameWithOwner: "Bookclerk/bookclerk" } }] },
              timelineItems: { nodes: [] },
            };
          }
          return Response.json({ data: { repository } });
        },
      ],
    ]);
    let runs = 0;
    for (; runs < 12; runs++) {
      const db = limitedD1(d1, 50);
      fetches = 0;
      await syncIssues({ ...env, DASHBOARD_DB: db }, githubConfig(env), NOW + runs);
      assert.ok(fetches <= 45, `run ${runs} made ${fetches} GitHub calls`);
      if ((await first(d1, "SELECT COUNT(*) AS n FROM issues")).n === 1150) break;
    }
    assert.ok(runs < 4, `the backlog drains in ${runs + 1} runs`);
    assert.equal((await first(d1, "SELECT COUNT(*) AS n FROM issues")).n, 1150);
    assert.equal((await first(d1, "SELECT COUNT(*) AS n FROM report_issue")).n, 1150);
    assert.equal((await first(d1, "SELECT value FROM sync_state WHERE key = 'issues_since'")).value, new Date(Date.parse(t(1149)) - 1000).toISOString());
    const linked = (await first(d1, "SELECT COUNT(*) AS n FROM issue_pr")).n;
    assert.equal(linked, (runs + 1) * 100, "each run refreshes its share of the queue");
    const written = await first(d1, "SELECT title FROM issue_pr WHERE issue_number = 1");
    assert.equal(written.title, "fix");
  });

  it("does not lose issues that updates shift between runs", async () => {
    const d1 = createD1();
    const env = makeEnv();
    const t = (i) => new Date(NOW - 86_400_000 + i * 1000).toISOString();
    const issues = Array.from({ length: 600 }, (_, i) => ({ number: i + 1, title: "x", state: "open", body: "", created_at: t(i), updated_at: t(i) }));
    gh = scriptGitHub([
      ...appRoutes(null),
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/issues?",
        ({ url }) => {
          const q = new URL(url).searchParams;
          const since = q.get("since") ?? "";
          const page = Number(q.get("page"));
          const listed = issues.filter((i) => i.updated_at >= since).sort((a, b) => a.updated_at.localeCompare(b.updated_at));
          return Response.json(listed.slice((page - 1) * 100, page * 100));
        },
      ],
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: {} } })],
    ]);
    await syncIssues({ ...env, DASHBOARD_DB: limitedD1(d1, 50) }, githubConfig(env), NOW);
    const first500 = (await first(d1, "SELECT COUNT(*) AS n FROM issues")).n;
    assert.ok(first500 < 600, "the backlog outlasts one run");
    // Issues already read are updated, moving them to the end of the listing.
    for (let i = 0; i < 200; i++) issues[i].updated_at = t(1000 + i);
    for (let run = 0; run < 3; run++) await syncIssues({ ...env, DASHBOARD_DB: limitedD1(d1, 50) }, githubConfig(env), NOW + run + 1);
    assert.equal((await first(d1, "SELECT COUNT(*) AS n FROM issues")).n, 600);
    assert.equal((await first(d1, "SELECT COUNT(*) AS n FROM issues WHERE updated_at = ?", Date.parse(t(1199)))).n, 1, "the updates were read too");
  });

  it("indexes stored reports over several presses within the limit", async () => {
    const env = makeEnv();
    const d1 = env.DASHBOARD_DB;
    const ids = Array.from({ length: 130 }, (_, i) => `00000000-0000-4000-8000-${String(i).padStart(12, "0")}`);
    const objects = {};
    const names = ids.map((id) => `diagnostics/0.9.0-rc.2/${id}.json`).sort();
    for (const id of ids) objects[`id:${id}`] = JSON.stringify(stored(id));
    await seed(env, ids[3]);
    let fetches = 0;
    const b2 = makeB2(objects);
    const download = b2.downloadById;
    b2.downloadById = (auth, id) => ((fetches += 1), download(auth, id));
    gh = scriptGitHub([
      [
        "POST https://b2.test/b2api/v2/b2_list_file_names",
        ({ body }) => {
          fetches += 1;
          const from = body.startFileName ? names.indexOf(body.startFileName) : 0;
          const files = names.slice(from, from + body.maxFileCount).map((fileName) => ({
            action: "upload",
            fileName,
            fileId: `id:${reportIdOf(fileName)}`,
            uploadTimestamp: NOW,
          }));
          return Response.json({ files, nextFileName: names[from + body.maxFileCount] ?? null });
        },
      ],
    ]);
    const cookie = await signedIn(env, "admin");
    let presses = 0;
    for (; presses < 6; presses++) {
      const db = limitedD1(d1, 50);
      fetches = 0;
      const res = await call({ ...env, DASHBOARD_DB: db }, new Request(`${ORIGIN}/admin/reindex`, { method: "POST", headers: { cookie, origin: ORIGIN } }), b2);
      assert.equal(res.status, 303);
      assert.ok(fetches <= 45, `press ${presses} made ${fetches} B2 calls`);
      const note = (await first(d1, "SELECT value FROM sync_state WHERE key = 'reindex_note'")).value;
      assert.doesNotMatch(note, /could not/);
      if (/finished/.test(note)) break;
    }
    assert.equal(presses, 3, "40 downloads a press");
    assert.equal((await first(d1, "SELECT COUNT(*) AS n FROM reports")).n, 130);
    assert.equal((await first(d1, "SELECT rows FROM index_days")).rows, 130, "the ledger counts each report once");
    assert.equal(await first(d1, "SELECT value FROM sync_state WHERE key = 'reindex_cursor'"), null);
  });
});
