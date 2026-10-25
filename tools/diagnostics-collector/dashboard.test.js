// Dashboard tests: sign-in, sessions, pages, index and issue sync, run under
// Node with a node:sqlite stand-in for D1 and a scripted GitHub.

import assert from "node:assert/strict";
import { generateKeyPairSync, verify as rsaVerify } from "node:crypto";
import { afterEach, beforeEach, describe, it } from "node:test";
import { OAUTH_COOKIE, pkceChallenge } from "./dashboard/auth.js";
import { all, fixStatus, getReport, indexReport, isRegressionVersion, first, linkBySignature, listIssues, run, sameVersionLine, topSignatures } from "./dashboard/db.js";
import { appJwt, clearTokenCache, githubConfig, INSTALLATION_PERMISSIONS, pkcs8FromPem } from "./dashboard/github.js";
import { escapeHtml, fmtTime, html, isoTime } from "./dashboard/html.js";
import { buckets } from "./dashboard/pages.js";
import { parseIssueReportIds, reportIdOf } from "./dashboard/report-ids.js";
import { dashboardFetch } from "./dashboard/routes.js";
import { SESSION_COOKIE, createSession } from "./dashboard/session.js";
import { normalizeMessage, summarizeReport } from "./dashboard/signature.js";
import { storeIssue, syncIssueNow, syncIssues } from "./dashboard/sync.js";
import { createD1 } from "./test-support/d1.js";
import worker from "./worker.js";

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
    assert.deepEqual(parseIssueReportIds(triage), { kind: "triage", ids: [ID1, ID2], regressionOf: null });
    const fallback = `<!-- bookclerk-diagnostics: triage fallback -->\n## Diagnostics batch\n\n### Report IDs\n- \`${ID2}\`\n\nAutomated triage did not cover these.`;
    assert.deepEqual(parseIssueReportIds(fallback), { kind: "fallback", ids: [ID2], regressionOf: null });
  });

  it("reads the issue a regression issue goes back to", () => {
    const body = `<!-- bookclerk-diagnostics: regression of #412 -->\n## Regression of #412\n\n### Report IDs\n- \`${ID1}\`\n`;
    assert.deepEqual(parseIssueReportIds(body), { kind: "regression", ids: [ID1], regressionOf: 412 });
    for (const bad of ["#0", "#012", "#1234567890", "# 12", "#12 ", "12"]) {
      const out = parseIssueReportIds(`<!-- bookclerk-diagnostics: regression of ${bad} -->\n### Report IDs\n- \`${ID1}\`\n`);
      assert.deepEqual([out.kind, out.regressionOf], ["other", null], bad);
    }
  });

  it("stores which closed issue a regression issue goes back to", async () => {
    const env = makeEnv();
    const base = { state: "open", created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-02T00:00:00Z" };
    const marker = (n) => `<!-- bookclerk-diagnostics: regression of #${n} -->\n## Regression of #${n}\n\n### Report IDs\n- \`${ID1}\`\n`;
    await storeIssue(env.DASHBOARD_DB, { ...base, number: 500, title: "[diagnostics] Regression of #412 in 0.9", body: marker(412) });
    // A marker naming a later issue cannot be a regression of it.
    await storeIssue(env.DASHBOARD_DB, { ...base, number: 501, title: "x", body: marker(900) });
    await storeIssue(env.DASHBOARD_DB, { ...base, number: 502, title: "x", body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n` });
    const rows = await all(env.DASHBOARD_DB, "SELECT number, kind, regression_of FROM issues WHERE number >= 500 ORDER BY number");
    assert.deepEqual(rows, [
      { number: 500, kind: "regression", regression_of: 412 },
      { number: 501, kind: "regression", regression_of: null },
      { number: 502, kind: "triage", regression_of: null },
    ]);
    // An edit that removes the marker clears it.
    await storeIssue(env.DASHBOARD_DB, { ...base, number: 500, title: "x", body: "edited", updated_at: "2026-10-03T00:00:00Z" });
    assert.equal((await all(env.DASHBOARD_DB, "SELECT regression_of FROM issues WHERE number = 500"))[0].regression_of, null);
  });

  it("reads IDs followed by a dashboard link", () => {
    const body = `### Report IDs\n- \`${ID1}\` · [dashboard](https://d.test/reports/${ID1})\n- \`${ID2}\` · [dashboard](https://d.test/a b)\n`;
    assert.deepEqual(parseIssueReportIds(body).ids, [ID1]);
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
      assert.equal(res.headers.get("location"), `/?next=${encodeURIComponent(p)}`);
    }
  });
});

describe("links from other sites", () => {
  /** Headers of a top-level navigation that began on another site. */
  const CROSS_SITE = { "sec-fetch-site": "cross-site", "sec-fetch-mode": "navigate", "sec-fetch-dest": "document", "sec-fetch-user": "?1" };

  it("reloads a page same-site instead of showing sign-in", async () => {
    const env = makeEnv();
    const target = `/reports/${ID1}`;
    const res = await call(env, new Request(ORIGIN + target, { headers: CROSS_SITE }));
    assert.equal(res.status, 200);
    assert.equal(res.headers.get("cache-control"), "no-store");
    assert.equal(res.headers.get("set-cookie"), null);
    const text = await res.text();
    assert.match(text, new RegExp(`<meta http-equiv="refresh" content="0;url=${target}">`));
    assert.doesNotMatch(text, /Sign in with GitHub/);
  });

  it("keeps a filter query and escapes it in the reload", async () => {
    const env = makeEnv();
    const res = await call(env, new Request(`${ORIGIN}/reports?version=0.9.0&page=2`, { headers: CROSS_SITE }));
    assert.equal(res.status, 200);
    assert.match(await res.text(), /content="0;url=\/reports\?version=0\.9\.0&amp;page=2"/);
  });

  it("reloads the home page with its return page", async () => {
    const env = makeEnv();
    const res = await call(env, new Request(`${ORIGIN}/?next=${encodeURIComponent(`/reports/${ID1}`)}`, { headers: CROSS_SITE }));
    assert.equal(res.status, 200);
    assert.match(await res.text(), new RegExp(`content="0;url=/\\?next=%2Freports%2F${ID1}"`));
  });

  it("shows sign-in on the same-site reload when there is no session", async () => {
    const env = makeEnv();
    const same = { "sec-fetch-site": "same-origin", "sec-fetch-mode": "navigate" };
    const res = await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers: same }));
    assert.equal(res.status, 303, "no second reload, so no loop");
    assert.equal(res.headers.get("location"), `/?next=%2Freports%2F${ID1}`);
    const home = await call(env, new Request(ORIGIN + res.headers.get("location"), { headers: same }));
    assert.equal(home.status, 200);
    assert.match(await home.text(), new RegExp(`href="/auth/github\\?next=%2Freports%2F${ID1}"`));
  });

  it("does not reload for scripted, framed or same-site loads", async () => {
    const env = makeEnv();
    const { "sec-fetch-user": _, ...scripted } = CROSS_SITE;
    for (const headers of [scripted, { ...CROSS_SITE, "sec-fetch-dest": "iframe" }, { ...CROSS_SITE, "sec-fetch-site": "same-site" }, {}]) {
      const res = await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers }));
      assert.equal(res.status, 303, JSON.stringify(headers));
      assert.equal(res.headers.get("location"), `/?next=%2Freports%2F${ID1}`);
    }
  });

  it("keeps a raw slash or colon in a filter query", async () => {
    const env = makeEnv();
    const res = await call(env, new Request(`${ORIGIN}/reports?platform=macos/arm64&version=1:2`));
    assert.equal(res.headers.get("location"), `/?next=${encodeURIComponent("/reports?platform=macos/arm64&version=1:2")}`);
  });

  it("does not reload sign-in, owner or WebSocket paths", async () => {
    const env = makeEnv();
    for (const p of ["/live", "/admin/reindex"]) {
      const res = await call(env, new Request(ORIGIN + p, { headers: CROSS_SITE }));
      assert.equal(res.status, 303, p);
      assert.equal(res.headers.get("location"), "/", p);
    }
    const start = await call(env, new Request(`${ORIGIN}/auth/github`, { headers: CROSS_SITE }));
    assert.match(start.headers.get("location"), /^https:\/\/github\.com\//);
  });

  it("sends a signed-in member from / to the return page, never off-site", async () => {
    const env = makeEnv();
    const cookie = await signedIn(env);
    const go = (next) => call(env, new Request(`${ORIGIN}/?next=${encodeURIComponent(next)}`, { headers: { cookie } }));
    assert.equal((await go(`/reports/${ID1}`)).headers.get("location"), `/reports/${ID1}`);
    for (const bad of ["//evil.test/dash", "https://evil.test/dash", "/\\evil.test", "/auth/github", "/admin/sign-out-all", "/dash/../auth/logout"]) {
      const res = await go(bad);
      assert.equal(res.status, 303, bad);
      assert.equal(res.headers.get("location"), "/dash", bad);
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

  it("carries a dashboard return page through the sign-in cookie", async () => {
    const res = await call(makeEnv(), new Request(`${ORIGIN}/auth/github?next=${encodeURIComponent(`/reports/${ID1}`)}`));
    const parts = res.headers.get("set-cookie").split(";")[0].split("=")[1].split(".");
    assert.equal(parts.length, 3);
    assert.equal(Buffer.from(parts[2], "base64url").toString("latin1"), `/reports/${ID1}`);
    for (const bad of ["https://evil.test/", "//evil.test", "/auth/callback"]) {
      const other = await call(makeEnv(), new Request(`${ORIGIN}/auth/github?next=${encodeURIComponent(bad)}`));
      assert.equal(other.headers.get("set-cookie").split(";")[0].split("=")[1].split(".").length, 2, bad);
    }
  });

  /** Runs a callback with a valid state cookie, optionally with a return page. */
  async function callback(env, ctx, next) {
    const state = "s".repeat(43);
    const verifier = "v".repeat(43);
    const tail = next === undefined ? "" : `.${Buffer.from(next).toString("base64url")}`;
    return call(
      env,
      new Request(`${ORIGIN}/auth/callback?code=abc123&state=${state}`, {
        headers: { cookie: `${OAUTH_COOKIE}=${state}.${verifier}${tail}` },
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

  it("lands on the return page after sign-in, or the overview when it is not a dashboard page", async () => {
    for (const [next, expected] of [[`/reports/${ID1}`, `/reports/${ID1}`], ["/issues?state=closed", "/issues?state=closed"], ["//evil.test", "/dash"]]) {
      clearTokenCache();
      gh = scriptGitHub([...userRoutes, ...appRoutes({ state: "active", role: "member", user: { id: 7 } })]);
      const ctx = makeCtx();
      const res = await callback(makeEnv(), ctx, next);
      await Promise.all(ctx.pending);
      assert.equal(res.status, 200, next);
      const text = await res.text();
      assert.ok(text.includes(`content="0;url=${expected}"`), next);
      assert.ok(text.includes(`href="${expected}"`), next);
      gh.restore();
    }
    gh = null;
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
    assert.match(issues, /href="\/issues\/412"/);
    assert.match(issues, /https:\/\/github.com\/Bookclerk\/bookclerk\/issues\/412/);
    const log = await all(env.DASHBOARD_DB, "SELECT action, report_id FROM access_log WHERE action = 'view-report'");
    assert.equal(log.length, 2);
    assert.equal(log[0].report_id, ID1);
  });

  it("counts a report filed twice under the older issue and flags the conflict", async () => {
    gh = scriptGitHub([]);
    const env = makeEnv();
    await seed(env, ID1);
    await seed(env, ID2);
    for (const number of [400, 403]) {
      await storeIssue(env.DASHBOARD_DB, {
        number,
        title: `[diagnostics] batch ${number}`,
        state: number === 400 ? "closed" : "open",
        body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n${number === 403 ? `- \`${ID2}\`\n` : ""}`,
        created_at: new Date(NOW - 86_400_000).toISOString(),
        updated_at: new Date(NOW - 86_400_000).toISOString(),
      });
    }
    const cookie = await signedIn(env);
    const list = await (await call(env, new Request(`${ORIGIN}/reports`, { headers: { cookie } }))).text();
    assert.match(list, /issues\/400">#400<\/a> <a class="quiet" href="\/reports\/[^"]+" title="Listed in 2 issues">\+1<\/a>/);
    assert.match(list, /issues\/403">#403<\/a><\/td>/, "the report only #403 lists shows no +1");
    const page = await (await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers: { cookie } }))).text();
    assert.match(page, /#400 \[diagnostics\] batch 400<\/a> <span class="fine">listed when filed<\/span>/);
    assert.match(page, /#403 \[diagnostics\] batch 403<\/a> <span class="fine">also lists it<\/span>/);
    assert.match(page, /Conflict<\/span> 2 issues list this report; it counts under #400/);
    const issues = await listIssues(env.DASHBOARD_DB, "all", 0, 10);
    assert.deepEqual(
      issues.map((i) => [i.number, Number(i.reports)]).sort(),
      [[400, 1], [403, 1]],
      "each report counts once, under the issue that tracks it",
    );
  });

  it("links a report to the open issue tracking its signature, on the same version line", async () => {
    gh = scriptGitHub([]);
    const env = makeEnv();
    await seed(env, ID1, NOW - 2 * 86_400_000);
    const same = "d2b80f13-0000-4000-8000-000000000003";
    const otherLine = "d2b80f13-0000-4000-8000-000000000004";
    const noSignature = "d2b80f13-0000-4000-8000-000000000005";
    await seed(env, same, NOW - 3_600_000, { version: "0.9.1" });
    await seed(env, otherLine, NOW - 3_600_000, { version: "0.10.0" });
    await seed(env, noSignature, NOW - 3_600_000, { events: [] });
    const issue = (number, state, marker = "workers-ai triage") => ({
      number,
      title: `[diagnostics] issue ${number}`,
      state,
      body: `<!-- bookclerk-diagnostics: ${marker} -->\nx\n\n### Report IDs\n- \`${ID1}\`\n`,
      created_at: new Date(NOW - 86_400_000).toISOString(),
      updated_at: new Date(NOW - 86_400_000).toISOString(),
    });
    // A closed issue and the holding issue track the signature too; neither attracts reports.
    await storeIssue(env.DASHBOARD_DB, issue(390, "closed"));
    await storeIssue(env.DASHBOARD_DB, issue(395, "open", "triage fallback"));
    await storeIssue(env.DASHBOARD_DB, issue(412, "open"));
    await storeIssue(env.DASHBOARD_DB, issue(413, "open"));
    const linked = await linkBySignature(env.DASHBOARD_DB, [same, otherLine, noSignature, ID1, "not-indexed"], NOW);
    assert.deepEqual([...linked], [[same, 412]], "the oldest open triaged issue wins");
    const rows = await all(env.DASHBOARD_DB, "SELECT * FROM report_issue WHERE report_id = ?", same);
    assert.deepEqual(rows, [{ report_id: same, issue_number: 412, source: "signature", linked_by: "ingest", linked_at: NOW }]);
    assert.deepEqual([...(await linkBySignature(env.DASHBOARD_DB, [same], NOW + 1))], [], "already linked");

    // The sync keeps the signature link while replacing the body's links,
    // and a body that comes to list the report makes the link `filed`.
    await storeIssue(env.DASHBOARD_DB, { ...issue(412, "open"), updated_at: new Date(NOW).toISOString() });
    assert.equal((await all(env.DASHBOARD_DB, "SELECT source FROM report_issue WHERE report_id = ?", same))[0].source, "signature");
    await storeIssue(env.DASHBOARD_DB, { ...issue(412, "open"), body: `${issue(412, "open").body}- \`${same}\`\n` });
    assert.equal((await all(env.DASHBOARD_DB, "SELECT source FROM report_issue WHERE report_id = ?", same))[0].source, "filed");
    // A report a member excluded from an issue is not linked to it again,
    // but may still join another issue tracking the signature on its line.
    const sixth = "d2b80f13-0000-4000-8000-000000000006";
    await seed(env, sixth, NOW - 60_000, { version: "0.10.1" });
    await storeIssue(env.DASHBOARD_DB, { ...issue(413, "open"), body: `${issue(413, "open").body}- \`${sixth}\`\n` });
    await run(env.DASHBOARD_DB, "UPDATE reports SET version = '0.10.0' WHERE report_id = ?", ID1);
    await run(env.DASHBOARD_DB, "INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 412, 'excluded')", otherLine);
    assert.deepEqual([...(await linkBySignature(env.DASHBOARD_DB, [otherLine], NOW))], [[otherLine, 413]], "excluded from #412, joins #413");

    // The holding issue never outranks a triaged issue that lists a report.
    const held = "d2b80f13-0000-4000-8000-000000000007";
    await seed(env, held);
    await storeIssue(env.DASHBOARD_DB, { ...issue(395, "open", "triage fallback"), body: `${issue(395, "open", "triage fallback").body}- \`${held}\`\n` });
    await storeIssue(env.DASHBOARD_DB, { ...issue(413, "open"), body: `${issue(413, "open").body}- \`${sixth}\`\n- \`${held}\`\n` });
    const tracked = new Map((await listIssues(env.DASHBOARD_DB, "all", 0, 10)).map((i) => [i.number, Number(i.reports)]));
    assert.equal(tracked.get(395), 0, "the held report counts under the triaged issue");
    assert.equal(tracked.get(413), 3);
    const heldPage = await getReport(env.DASHBOARD_DB, held);
    assert.equal(heldPage.report.issue_number, 413);
    assert.deepEqual(heldPage.issues.map((i) => i.number), [413, 395], "the report page lists the tracker first");
    assert.equal(sameVersionLine("v1.2.3", "1.2.0-rc.1"), true);
    assert.equal(sameVersionLine("1.2", "1.20"), false);
    assert.equal(sameVersionLine("1.2.3", ""), false);
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
    const after = await fixStatus(env.DASHBOARD_DB);
    assert.equal(after.get(412).after, 1, "the newer report shares the signature and the fix is unreleased");
    assert.equal(after.get(412).source, null, "an open issue's fix is not resolved against releases");
    const list = gh.calls.find((c) => c.url.includes("/issues?"));
    assert.match(list.url, /labels=diagnostics/);
  });
});

describe("fixed-in version", () => {
  const SHA = "a".repeat(40);
  const ID3 = "d2b80f13-0000-4000-8000-000000000003";

  /** A closed issue listing ID1, with the given labels. */
  function closedIssue(number, labels = [{ name: "diagnostics" }]) {
    return {
      number,
      title: "[diagnostics] Libro stalls",
      state: "closed",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n`,
      created_at: new Date(NOW - 6 * 86_400_000).toISOString(),
      updated_at: new Date(NOW - 2 * 86_400_000).toISOString(),
      closed_at: new Date(NOW - 2 * 86_400_000).toISOString(),
      labels,
    };
  }

  /** GraphQL answer: one merged pull request with a merge commit. */
  const mergedPr = (mergedAt) =>
    Response.json({
      data: {
        repository: {
          i0: {
            labels: { nodes: [{ name: "diagnostics" }], pageInfo: { hasNextPage: false } },
            closedByPullRequestsReferences: {
              nodes: [{ number: 418, title: "Retry", state: "MERGED", mergedAt: new Date(mergedAt).toISOString(), mergeCommit: { oid: SHA }, repository: { nameWithOwner: "Bookclerk/bookclerk" } }],
            },
            timelineItems: { nodes: [] },
          },
        },
      },
    });

  it("records the first release that contains the fix and counts regressions by version", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0-rc.2" });
    await seed(env, ID2, NOW - 3_600_000, { version: "0.9.2+nix" });
    await seed(env, ID3, NOW - 1_800_000, { version: "0.9.1" });
    const release = (tag, at) => ({ tag_name: tag, draft: false, prerelease: tag.includes("-"), published_at: new Date(at).toISOString() });
    let releaseCalls = 0;
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(412)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/releases",
        () => {
          releaseCalls += 1;
          return Response.json([release("v0.9.2", NOW - 12 * 3_600_000), release("v0.9.1", NOW - 86_400_000), release("v0.9.0", NOW - 10 * 86_400_000)]);
        },
      ],
      [`GET https://api.github.com/repos/Bookclerk/bookclerk/compare/v0.9.1...${SHA}`, () => new Response("{}", { status: 404 })],
      [`GET https://api.github.com/repos/Bookclerk/bookclerk/compare/v0.9.2...${SHA}`, () => Response.json({ status: "behind" })],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    const rows = await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix");
    assert.equal(rows.length, 1);
    assert.deepEqual(
      [rows[0].issue_number, rows[0].source, rows[0].fixed_in, rows[0].pr_number, rows[0].merge_sha, rows[0].recorded_by],
      [412, "release", "0.9.2", 418, SHA, "sync"],
    );
    assert.ok(!gh.calls.some((c) => c.url.includes("compare/v0.9.0...")), "releases before the merge are not compared");
    const status = await fixStatus(env.DASHBOARD_DB);
    assert.equal(status.get(412).fixedIn, "0.9.2");
    assert.equal(status.get(412).after, 1, "only the 0.9.2 repack counts; 0.9.1 and the rc are older than the fix");
    assert.equal((await all(env.DASHBOARD_DB, "SELECT merge_sha FROM issue_pr"))[0].merge_sha, SHA);

    // A resolved issue is not looked up again.
    await syncIssues(env, githubConfig(env), NOW + 60_000);
    assert.equal(releaseCalls, 1);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix")).length, 1);

    const cookie = await signedIn(env);
    const page = await (await call(env, new Request(`${ORIGIN}/reports/${ID2}`, { headers: { cookie } }), makeB2({ [`id:diagnostics/x/${ID2}.json`]: JSON.stringify(stored(ID2)) }))).text();
    assert.match(page, /Regression/);
    assert.match(page, /fixed in 0\.9\.2/);
    const older = await (await call(env, new Request(`${ORIGIN}/reports/${ID3}`, { headers: { cookie } }), makeB2({ [`id:diagnostics/x/${ID3}.json`]: JSON.stringify(stored(ID3)) }))).text();
    assert.doesNotMatch(older, /chip crit">Regression/);
    const issues = await (await call(env, new Request(`${ORIGIN}/issues?state=closed`, { headers: { cookie } }))).text();
    assert.match(issues, /1 regressions/);
  });

  it("waits for a release that contains the fix, and lets a fixed-in label decide", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0" });
    await seed(env, ID2, NOW - 3_600_000, { version: "0.9.0" });
    const releases = [{ tag_name: "v0.9.0", draft: false, prerelease: false, published_at: new Date(NOW - 10 * 86_400_000).toISOString() }];
    let issue = closedIssue(413);
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([issue])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/releases", () => Response.json(releases)],
      [`GET https://api.github.com/repos/Bookclerk/bookclerk/compare/`, ({ url }) => Response.json({ status: url.includes("v0.9.1...") ? "behind" : "ahead" })],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    let rows = await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at");
    assert.deepEqual(rows, [{ source: "unreleased", fixed_in: null }], "no release after the merge yet");
    let status = await fixStatus(env.DASHBOARD_DB);
    assert.equal(status.get(413).source, "unreleased");
    assert.equal(status.get(413).after, 1, "while unreleased, reports after the merge count");

    // No newer release: not looked at again.
    await syncIssues(env, githubConfig(env), NOW + 60_000);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix")).length, 1);

    // A release appears that contains the fix.
    releases.unshift({ tag_name: "v0.9.1", draft: false, prerelease: false, published_at: new Date(NOW + 3_600_000).toISOString() });
    await syncIssues(env, githubConfig(env), NOW + 7_200_000);
    rows = await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at");
    assert.deepEqual(rows, [{ source: "unreleased", fixed_in: null }, { source: "release", fixed_in: "0.9.1" }]);

    // A member's label overrides it, and the sync leaves the issue alone afterwards.
    issue = { ...closedIssue(413, [{ name: "diagnostics" }, { name: "fixed-in:v0.9.0" }]), updated_at: new Date(NOW + 3 * 3_600_000).toISOString() };
    await syncIssues(env, githubConfig(env), NOW + 4 * 3_600_000);
    rows = await all(env.DASHBOARD_DB, "SELECT source, fixed_in, recorded_by FROM issue_fix ORDER BY recorded_at");
    assert.deepEqual(rows.at(-1), { source: "manual", fixed_in: "0.9.0", recorded_by: "label" });
    await syncIssues(env, githubConfig(env), NOW + 5 * 3_600_000);
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix")).length, 3, "the same label writes no second row");
    status = await fixStatus(env.DASHBOARD_DB);
    assert.equal(status.get(413).fixedIn, "0.9.0");
    assert.equal(status.get(413).source, "manual");
    assert.equal(status.get(413).after, 2, "both 0.9.0 reports are regressions against the labeled version");

    // The label is removed: its rows go, and the release row counts again.
    issue = { ...closedIssue(413), updated_at: new Date(NOW + 6 * 3_600_000).toISOString() };
    await syncIssues(env, githubConfig(env), NOW + 7 * 3_600_000);
    rows = await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at");
    assert.deepEqual(rows, [{ source: "unreleased", fixed_in: null }, { source: "release", fixed_in: "0.9.1" }]);
    status = await fixStatus(env.DASHBOARD_DB);
    assert.equal(status.get(413).fixedIn, "0.9.1");
    assert.equal(status.get(413).after, 0);
  });

  it("looks past releases it already compared and ignores tags that are not versions", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0" });
    const at = (hours) => new Date(NOW + hours * 3_600_000).toISOString();
    const tag = (tag_name, published_at, draft = false) => ({ tag_name, draft, prerelease: false, published_at });
    // Three maintenance releases after the merge from another branch, and a
    // nightly tag, none containing the fix.
    const releases = [tag("nightly-2026-10-24", at(-30)), tag("v0.8.7", at(-40)), tag("v0.8.6", at(-41)), tag("v0.8.5", at(-42)), tag("v0.8.4", at(-240))];
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(415)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/releases", () => Response.json(releases)],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          const t = /compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1];
          compared.push(t);
          return Response.json({ status: t === "v0.9.0" ? "behind" : "diverged" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(compared, ["v0.8.5", "v0.8.6", "v0.8.7"], "the nightly tag is not a version; three releases are compared, oldest first");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix"), [{ source: "unreleased", fixed_in: null }]);

    // The release that contains the fix comes later than the three compared.
    releases.unshift(tag("v0.9.0", at(1)));
    compared.length = 0;
    await syncIssues(env, githubConfig(env), NOW + 2 * 3_600_000);
    assert.deepEqual(compared, ["v0.9.0"], "only releases newer than the last look are compared");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at"), [{ source: "unreleased", fixed_in: null }, { source: "release", fixed_in: "0.9.0" }]);
  });

  it("continues with the fourth release on the next sync when more than three follow the merge", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0" });
    const at = (hours) => new Date(NOW + hours * 3_600_000).toISOString();
    const tag = (tag_name, published_at) => ({ tag_name, draft: false, prerelease: false, published_at });
    // Four releases after the merge; only the fourth (newest) contains the fix.
    const releases = [tag("v0.9.0", at(-10)), tag("v0.8.7", at(-20)), tag("v0.8.6", at(-30)), tag("v0.8.5", at(-40))];
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(416)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/releases", () => Response.json(releases)],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          const t = /compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1];
          compared.push(t);
          return Response.json({ status: t === "v0.9.0" ? "behind" : "diverged" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(compared, ["v0.8.5", "v0.8.6", "v0.8.7"], "the per-sync comparison budget stops before the fourth release");
    const [first] = await all(env.DASHBOARD_DB, "SELECT source, fixed_in, checked_until FROM issue_fix");
    assert.equal(first.source, "unreleased");
    assert.equal(first.checked_until, Date.parse(at(-20)), "the cursor records the last release compared, not the newest one");

    // No new release has appeared, yet the next sync must still look at v0.9.0.
    compared.length = 0;
    await syncIssues(env, githubConfig(env), NOW + 3_600_000);
    assert.deepEqual(compared, ["v0.9.0"]);
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at"), [{ source: "unreleased", fixed_in: null }, { source: "release", fixed_in: "0.9.0" }]);
  });

  it("shows the tracking issue's fix on the report page, not another linked issue's", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.5" });
    // Two closed issues list the report; the lower number tracks it and has
    // no fix, the other has a fix shipped in 0.9.2.
    const issue = (prs) => ({
      labels: { nodes: [{ name: "diagnostics" }], pageInfo: { hasNextPage: false } },
      closedByPullRequestsReferences: { nodes: prs },
      timelineItems: { nodes: [] },
    });
    const pr = { number: 418, title: "Retry", state: "MERGED", mergedAt: new Date(NOW - 2 * 86_400_000).toISOString(), mergeCommit: { oid: SHA }, repository: { nameWithOwner: "Bookclerk/bookclerk" } };
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(440), closedIssue(441)])],
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: { i0: issue([]), i1: issue([pr]) } } })],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/releases",
        () => Response.json([{ tag_name: "v0.9.2", draft: false, prerelease: false, published_at: new Date(NOW - 86_400_000).toISOString() }]),
      ],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/compare/", () => Response.json({ status: "behind" })],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT issue_number, fixed_in FROM issue_fix"), [{ issue_number: 441, fixed_in: "0.9.2" }]);
    const report = await getReport(env.DASHBOARD_DB, ID1);
    assert.equal(report.report.issue_number, 440, "the lower-numbered issue tracks the report");

    const cookie = await signedIn(env);
    const page = await (await call(env, new Request(`${ORIGIN}/reports/${ID1}`, { headers: { cookie } }), makeB2({ [`id:diagnostics/x/${ID1}.json`]: JSON.stringify(stored(ID1)) }))).text();
    assert.doesNotMatch(page, /chip crit">Regression/, "the tracking issue has no fix, so the report is not a regression");
    assert.doesNotMatch(page, /fixed in 0\.9\.2/);
  });

  it("tells releases published in the same second apart by tag when it continues", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0" });
    const same = new Date(NOW - 3_600_000).toISOString();
    // Four releases published in the same second (one automated run), only
    // the last by tag contains the fix.
    const releases = ["v0.9.3", "v0.9.2", "v0.9.1", "v0.9.0"].map((tag_name) => ({ tag_name, draft: false, prerelease: false, published_at: same }));
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(432)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/releases", () => Response.json(releases)],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          const t = /compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1];
          compared.push(t);
          return Response.json({ status: t === "v0.9.3" ? "behind" : "diverged" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(compared, ["v0.9.0", "v0.9.1", "v0.9.2"]);
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, checked_until, checked_tag FROM issue_fix"), [{ source: "unreleased", checked_until: Date.parse(same), checked_tag: "v0.9.2" }]);

    compared.length = 0;
    await syncIssues(env, githubConfig(env), NOW + 3_600_000);
    assert.deepEqual(compared, ["v0.9.3"], "the tag after the cursor's tag is compared, not skipped");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix ORDER BY recorded_at"), [{ source: "unreleased", fixed_in: null }, { source: "release", fixed_in: "0.9.3" }]);
  });

  it("resolves an issue closed late with an early merge once the five ahead of it are caught up", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "0.9.0" });
    // Five issues closed early with fixes merged after the only release
    // (nothing to compare, so they stay unreleased), and a sixth closed
    // later whose fix merged long before that release.
    const early = [450, 451, 452, 453, 454];
    const issue = (mergedAt) => ({
      labels: { nodes: [{ name: "diagnostics" }], pageInfo: { hasNextPage: false } },
      closedByPullRequestsReferences: {
        nodes: [{ number: 418, title: "Retry", state: "MERGED", mergedAt: new Date(mergedAt).toISOString(), mergeCommit: { oid: SHA }, repository: { nameWithOwner: "Bookclerk/bookclerk" } }],
      },
      timelineItems: { nodes: [] },
    });
    const late = { ...closedIssue(455), closed_at: new Date(NOW - 86_400_000).toISOString() };
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([...early.map((n) => closedIssue(n)), late])],
      [
        "POST https://api.github.com/graphql",
        ({ body }) => {
          const numbers = [...String(body.query).matchAll(/issue\(number: (\d+)\)/g)].map((m) => Number(m[1]));
          const repository = Object.fromEntries(numbers.map((n, i) => [`i${i}`, issue(n === 455 ? NOW - 20 * 86_400_000 : NOW - 2 * 86_400_000)]));
          return Response.json({ data: { repository } });
        },
      ],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/releases",
        () => Response.json([{ tag_name: "v0.9.0", draft: false, prerelease: false, published_at: new Date(NOW - 10 * 86_400_000).toISOString() }]),
      ],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          compared.push(/compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1]);
          return Response.json({ status: "behind" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(compared, [], "the five early issues have no release after their merge");
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix WHERE source = 'unreleased'")).length, 5);

    await syncIssues(env, githubConfig(env), NOW + 60_000);
    assert.deepEqual(compared, ["v0.9.0"], "the sixth issue is resolved once the five ahead of it are caught up");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT issue_number, fixed_in FROM issue_fix WHERE source = 'release'"), [{ issue_number: 455, fixed_in: "0.9.0" }]);
  });

  it("pages through the release list back to the merge before choosing the first release", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "1.0.0" });
    // 150 releases, one an hour, all newer than the merge: the first one
    // after the merge is on the second page.
    const releases = Array.from({ length: 150 }, (_, i) => ({
      tag_name: `v1.0.${149 - i}`,
      draft: false,
      prerelease: false,
      published_at: new Date(NOW - (i + 1) * 3_600_000).toISOString(),
    }));
    const pages = [];
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(417)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 200 * 3_600_000)],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/releases",
        ({ url }) => {
          const u = new URL(url);
          const page = Number(u.searchParams.get("page"));
          const per = Number(u.searchParams.get("per_page"));
          pages.push(page);
          return Response.json(releases.slice((page - 1) * per, page * per));
        },
      ],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          compared.push(/compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1]);
          return Response.json({ status: "behind" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(pages, [1, 2], "the list is read to its end");
    assert.deepEqual(compared, ["v1.0.0"], "the oldest release after the merge is compared first");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT source, fixed_in FROM issue_fix"), [{ source: "release", fixed_in: "1.0.0" }]);
  });

  it("resolves nothing when the release list is longer than it reads", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000, { version: "1.0.0" });
    // More releases than five pages hold: a release published late could
    // sit past the pages read, so no fix is placed in a release.
    const releases = Array.from({ length: 520 }, (_, i) => ({
      tag_name: `v1.0.${519 - i}`,
      draft: false,
      prerelease: false,
      published_at: new Date(NOW - (i + 1) * 3_600_000).toISOString(),
    }));
    const pages = [];
    const compared = [];
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(418), closedIssue(419)])],
      [
        "POST https://api.github.com/graphql",
        () => {
          const issue = (number, mergedAt) => ({
            labels: { nodes: [{ name: "diagnostics" }], pageInfo: { hasNextPage: false } },
            closedByPullRequestsReferences: {
              nodes: [{ number, title: "Retry", state: "MERGED", mergedAt: new Date(mergedAt).toISOString(), mergeCommit: { oid: SHA }, repository: { nameWithOwner: "Bookclerk/bookclerk" } }],
            },
            timelineItems: { nodes: [] },
          });
          return Response.json({ data: { repository: { i0: issue(430, NOW - 600 * 3_600_000), i1: issue(431, NOW - 10 * 3_600_000 - 1) } } });
        },
      ],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/releases",
        ({ url }) => {
          const u = new URL(url);
          const page = Number(u.searchParams.get("page"));
          const per = Number(u.searchParams.get("per_page"));
          pages.push(page);
          return Response.json(releases.slice((page - 1) * per, page * per));
        },
      ],
      [
        "GET https://api.github.com/repos/Bookclerk/bookclerk/compare/",
        ({ url }) => {
          compared.push(/compare\/([^.]+(?:\.[^.]+)*?)\.\.\./.exec(url)[1]);
          return Response.json({ status: "behind" });
        },
      ],
    ]);
    await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(pages, [1, 2, 3, 4, 5], "the lookup stops after five pages");
    assert.deepEqual(compared, [], "no release is compared against an incomplete list");
    assert.deepEqual(await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix"), []);
  });

  it("keeps the rows when GitHub's release reads fail", async () => {
    const env = makeEnv();
    await seed(env, ID1, NOW - 5 * 86_400_000);
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([closedIssue(414)])],
      ["POST https://api.github.com/graphql", () => mergedPr(NOW - 2 * 86_400_000)],
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/releases", () => new Response("down", { status: 502 })],
    ]);
    const res = await syncIssues(env, githubConfig(env), NOW);
    assert.deepEqual(res, { issues: 1, refreshed: 1 });
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issue_fix")).length, 0);
  });

  it("orders versions the way a regression needs", () => {
    const cases = [
      ["0.9.2", "0.9.2", true],
      ["v0.9.2", "0.9.2", true],
      ["0.9.2+nix", "0.9.2", true],
      ["0.9.2-1", "0.9.2", true],
      ["0.9.2-rc.1", "0.9.2", false],
      ["0.9.3-rc.1", "0.9.2", true],
      ["0.10.0", "0.9.2", true],
      ["0.9.1", "0.9.2", false],
      ["0.0.0-synthetic", "0.9.2", false],
      ["garbage", "0.9.2", false],
      ["", "0.9.2", false],
    ];
    for (const [version, fixedIn, expected] of cases) assert.equal(isRegressionVersion(version, fixedIn), expected, `${version} vs ${fixedIn}`);
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

describe("owner issue sync", () => {
  it("links a filed issue to its report right away, within the D1 limit", async () => {
    const env = makeEnv();
    const d1 = env.DASHBOARD_DB;
    await seed(env, ID1);
    const issue = {
      number: 400,
      title: "[diagnostics] synthetic",
      state: "closed",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n`,
      created_at: new Date(NOW - 60_000).toISOString(),
      updated_at: new Date(NOW - 30_000).toISOString(),
      closed_at: new Date(NOW - 30_000).toISOString(),
    };
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues?", () => Response.json([issue])],
      ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: { i0: { closedByPullRequestsReferences: { nodes: [] }, timelineItems: { nodes: [] } } } } })],
    ]);
    const member = await signedIn(env, "member");
    const refused = await call(env, new Request(`${ORIGIN}/admin/sync`, { method: "POST", headers: { cookie: member, origin: ORIGIN } }));
    assert.equal(refused.status, 403);
    const owner = await signedIn(env, "admin");
    const res = await call({ ...env, DASHBOARD_DB: limitedD1(d1, 50) }, new Request(`${ORIGIN}/admin/sync`, { method: "POST", headers: { cookie: owner, origin: ORIGIN } }));
    assert.equal(res.status, 303);
    assert.deepEqual((await all(d1, "SELECT report_id, issue_number FROM report_issue")).map((r) => [r.report_id, r.issue_number]), [[ID1, 400]]);
    assert.match((await first(d1, "SELECT value FROM sync_state WHERE key = 'reindex_note'")).value, /stored 1 changed issues/);
    assert.equal((await first(d1, "SELECT action FROM access_log ORDER BY id DESC LIMIT 1")).action, "sync");
  });
});

describe("issue notice from the ingest Action", () => {
  const KEY = "r".repeat(32);
  /** POSTs a notice to the Worker. */
  function notify(env, body, key = KEY) {
    return worker.fetch(
      new Request(`${ORIGIN}/issues/notify`, {
        method: "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body,
      }),
      env,
      makeCtx(),
    );
  }
  const filed = (labels) => ({
    number: 400,
    title: "[diagnostics] synthetic",
    state: "open",
    labels,
    body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n- \`${ID1}\`\n`,
    created_at: new Date(NOW - 60_000).toISOString(),
    updated_at: new Date(NOW - 30_000).toISOString(),
    closed_at: null,
  });
  const noPrs = ["POST https://api.github.com/graphql", () => Response.json({ data: { repository: { i0: { closedByPullRequestsReferences: { nodes: [] }, timelineItems: { nodes: [] } } } } })];

  it("links the issue's reports from GitHub's copy, not the caller's", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400", () => Response.json(filed([{ name: "diagnostics" }]))],
      noPrs,
    ]);
    const res = await notify(env, JSON.stringify({ issue: 400, report_ids: [ID2] }));
    assert.equal(res.status, 202);
    assert.deepEqual(await res.json(), { ok: true, issue: 400, result: "linked" });
    assert.deepEqual((await all(env.DASHBOARD_DB, "SELECT report_id, issue_number FROM report_issue")).map((r) => [r.report_id, r.issue_number]), [[ID1, 400]]);
    assert.ok((await first(env.DASHBOARD_DB, "SELECT prs_synced_at FROM issues")).prs_synced_at > 0);
  });

  it("forgets an issue without the diagnostics label", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    await storeIssue(env.DASHBOARD_DB, filed([]));
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400", () => Response.json(filed([{ name: "bug" }]))],
    ]);
    const res = await notify(env, JSON.stringify({ issue: 400 }));
    assert.equal((await res.json()).result, "dropped");
    assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM report_issue")).length, 0);
  });

  it("ignores a transferred issue and one GitHub answers with another number", async () => {
    for (const answer of [
      () => new Response(null, { status: 301, headers: { location: "https://api.github.com/repositories/9/issues/7" } }),
      () => Response.json({ ...filed([{ name: "diagnostics" }]), number: 7 }),
    ]) {
      const env = makeEnv({ REPORT_API_KEY: KEY });
      gh = scriptGitHub([...appRoutes(null), ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400", answer]]);
      assert.equal(await syncIssueNow(env, githubConfig(env), 400, NOW, []), "dropped");
      assert.equal((await all(env.DASHBOARD_DB, "SELECT * FROM issues")).length, 0);
      gh.restore();
      gh = null;
      clearTokenCache();
    }
  });

  it("reads again when GitHub does not have the new issue yet", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    let reads = 0;
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400", () => (++reads === 1 ? new Response("{}", { status: 404 }) : Response.json(filed([{ name: "diagnostics" }])))],
      noPrs,
    ]);
    assert.equal(await syncIssueNow(env, githubConfig(env), 400, NOW, [0]), "linked");
    assert.equal(reads, 2);
  });

  it("pushes a live update only when the notice changed the links", async () => {
    const pushes = [];
    const LIVE = { idFromName: (n) => n, get: () => ({ fetch: async (url) => (pushes.push(String(url)), new Response(null, { status: 204 })) }) };
    const env = makeEnv({ REPORT_API_KEY: KEY, LIVE });
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400", () => Response.json(filed([{ name: "diagnostics" }]))],
      noPrs,
    ]);
    assert.equal((await notify(env, JSON.stringify({ issue: 400 }))).status, 202);
    assert.deepEqual(pushes, ["https://live/changed"]);
    assert.equal((await notify(env, JSON.stringify({ issue: 400 }))).status, 202);
    assert.equal(pushes.length, 1);
  });

  it("refuses a missing key or a malformed body, and answers 204 when the dashboard is off", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    assert.equal((await notify(env, JSON.stringify({ issue: 400 }), "x".repeat(32))).status, 401);
    for (const body of ["", "[]", '{"issue":"400"}', '{"issue":0}', '{"issue":1.5}', "x".repeat(300)]) {
      assert.equal((await notify(env, body)).status, 400, body.slice(0, 20));
    }
    const off = await notify({ REPORT_API_KEY: KEY }, JSON.stringify({ issue: 400 }));
    assert.equal(off.status, 204);
  });
});

describe("issue summaries for the ingest Action", () => {
  const KEY = "r".repeat(32);
  const ID3 = "d2b80f13-0000-4000-8000-000000000003";
  const ID4 = "d2b80f13-0000-4000-8000-000000000004";

  /** Calls the summaries route or the done route with the report key. */
  function call(env, path, body, key = KEY) {
    return worker.fetch(
      new Request(`${ORIGIN}${path}`, {
        method: body === undefined ? "GET" : "POST",
        headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
        body,
      }),
      env,
      makeCtx(),
    );
  }

  /** Two issues: #400 tracks three reports, #401 tracks none. */
  async function setup() {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    await seed(env, ID1, NOW - 3 * 86_400_000, { version: "0.9.1", os: "linux", arch: "x86_64" });
    await seed(env, ID2, NOW - 86_400_000, { version: "0.9.2", os: "windows", arch: "x86_64" });
    await seed(env, ID3, NOW - 3_600_000, { version: "0.9.2", os: "linux", arch: "x86_64" });
    await seed(env, ID4, NOW - 60_000, { version: "0.9.2" });
    const issue = (number) => ({
      number,
      title: `[diagnostics] issue ${number}`,
      state: "open",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n${number === 400 ? `- \`${ID1}\`\n` : ""}`,
      created_at: new Date(NOW - 4 * 86_400_000).toISOString(),
      updated_at: new Date(NOW - 4 * 86_400_000).toISOString(),
    });
    await storeIssue(env.DASHBOARD_DB, issue(400));
    await storeIssue(env.DASHBOARD_DB, issue(401));
    await run(env.DASHBOARD_DB, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 400, 'signature', 'ingest', ?)", ID2, NOW);
    await run(env.DASHBOARD_DB, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 400, 'manual', 'octocat', ?)", ID3, NOW);
    return env;
  }

  it("hands out the figures of issues whose summary is out of date, newest activity first", async () => {
    const env = await setup();
    const res = await call(env, "/issues/summaries");
    assert.equal(res.status, 200);
    const { issues } = await res.json();
    assert.deepEqual(issues.map((i) => i.issue), [400], "an issue tracking nothing, never summarized, is left out");
    const s = issues[0];
    assert.match(s.fingerprint, /^[0-9a-f]{64}$/);
    assert.equal(s.reports, 3);
    assert.equal(s.first_seen, NOW - 3 * 86_400_000);
    assert.equal(s.last_seen, NOW - 3_600_000);
    assert.deepEqual(s.sources, { filed: 1, signature: 1, triage: 0, manual: 1 });
    assert.deepEqual(s.versions, [{ version: "0.9.2", n: 2 }, { version: "0.9.1", n: 1 }]);
    assert.deepEqual(s.platforms, [{ platform: "linux x86_64", n: 2 }, { platform: "windows x86_64", n: 1 }]);
    assert.deepEqual(s.newest.map((r) => r.report_id), [ID3, ID2, ID1]);
    assert.equal(s.state, undefined, "state is not shown, so it is not hashed");
  });

  it("stops handing out an issue once its fingerprint is recorded, until the figures change", async () => {
    const env = await setup();
    const { issues } = await (await call(env, "/issues/summaries")).json();
    const done = await call(env, "/issues/summary", JSON.stringify({ issue: 400, fingerprint: issues[0].fingerprint }));
    assert.equal(done.status, 200);
    assert.deepEqual((await (await call(env, "/issues/summaries")).json()).issues, []);
    // A member detaches a report: the figures, and so the fingerprint, change.
    await run(env.DASHBOARD_DB, "UPDATE report_issue SET source = 'excluded' WHERE report_id = ?", ID3);
    const again = (await (await call(env, "/issues/summaries")).json()).issues;
    assert.deepEqual(again.map((i) => [i.issue, i.reports, i.sources.manual]), [[400, 2, 0]]);
    assert.notEqual(again[0].fingerprint, issues[0].fingerprint);
    // Detaching the last report yields one empty summary, then silence.
    await run(env.DASHBOARD_DB, "UPDATE report_issue SET source = 'excluded' WHERE issue_number = 400");
    const empty = (await (await call(env, "/issues/summaries")).json()).issues;
    assert.deepEqual(empty.map((i) => [i.issue, i.reports, i.newest.length]), [[400, 0, 0]]);
    await call(env, "/issues/summary", JSON.stringify({ issue: 400, fingerprint: empty[0].fingerprint }));
    assert.deepEqual((await (await call(env, "/issues/summaries")).json()).issues, []);
  });

  it("summarizes the holding issue for untriaged reports like any other issue", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    await seed(env, ID3, NOW - 60_000, { version: "0.9.2" });
    await storeIssue(env.DASHBOARD_DB, {
      number: 410,
      title: "[diagnostics] untriaged reports",
      state: "open",
      body: `<!-- bookclerk-diagnostics: triage fallback -->\n## Untriaged reports\n\n<!-- bookclerk-diagnostics: report ids\n${ID3}\n-->\n`,
      created_at: new Date(NOW).toISOString(),
      updated_at: new Date(NOW).toISOString(),
    });
    const { issues } = await (await call(env, "/issues/summaries")).json();
    assert.deepEqual(issues.map((i) => [i.issue, i.reports, i.sources.filed]), [[410, 1, 1]], "its body shows no IDs, so the comment is the list");
  });

  it("folds long breakdowns into others and lists the ten newest reports", async () => {
    const env = makeEnv({ REPORT_API_KEY: KEY });
    const ids = [];
    for (let i = 0; i < 12; i++) {
      const id = `d2b80f13-0000-4000-8000-0000000000${String(i + 10).padStart(2, "0")}`;
      ids.push(id);
      await seed(env, id, NOW - i * 60_000, { version: `0.${i}.0` });
    }
    await storeIssue(env.DASHBOARD_DB, {
      number: 400,
      title: "x",
      state: "open",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n${ids.map((i) => `- \`${i}\`\n`).join("")}`,
      created_at: new Date(NOW).toISOString(),
      updated_at: new Date(NOW).toISOString(),
    });
    const { issues } = await (await call(env, "/issues/summaries")).json();
    assert.equal(issues[0].versions.length, 9);
    assert.deepEqual(issues[0].versions[8], { version: "others", n: 4 });
    assert.deepEqual(issues[0].newest.map((r) => r.report_id), ids.slice(0, 10));
  });

  it("refuses a missing key or a malformed body, and answers 204 when the dashboard is off", async () => {
    const env = await setup();
    assert.equal((await call(env, "/issues/summaries", undefined, "x".repeat(32))).status, 401);
    assert.equal((await call({ REPORT_API_KEY: KEY }, "/issues/summaries")).status, 204);
    const fp = "a".repeat(64);
    for (const body of ["", "[]", '{"issue":"400"}', `{"issue":400,"fingerprint":"${"A".repeat(64)}"}`, '{"issue":400}', "x".repeat(600)]) {
      assert.equal((await call(env, "/issues/summary", body)).status, 400, body.slice(0, 30));
    }
    assert.equal((await call(env, "/issues/summary", JSON.stringify({ issue: 999, fingerprint: fp }))).status, 404);
    assert.equal((await call({ REPORT_API_KEY: KEY }, "/issues/summary", JSON.stringify({ issue: 400, fingerprint: fp }))).status, 204);
  });
});

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

describe("issue page and ledger corrections", () => {
  const ID3 = "d2b80f13-0000-4000-8000-000000000003";
  const ID4 = "d2b80f13-0000-4000-8000-000000000004";

  /** Stores an open triaged issue listing the given reports. */
  async function openIssue(env, number, ids) {
    await storeIssue(env.DASHBOARD_DB, {
      number,
      title: `[diagnostics] issue ${number}`,
      state: "open",
      body: `<!-- bookclerk-diagnostics: workers-ai triage -->\nx\n\n### Report IDs\n${ids.map((id) => `- \`${id}\`\n`).join("")}`,
      created_at: new Date(NOW - 86_400_000).toISOString(),
      updated_at: new Date(NOW - 86_400_000).toISOString(),
    });
  }

  /** Posts a correction form as the member with the given cookie. */
  function correct(env, cookie, fields, headers = { origin: ORIGIN }) {
    return call(env, new Request(`${ORIGIN}/issues/link`, {
      method: "POST",
      headers: { cookie, "content-type": "application/x-www-form-urlencoded", ...headers },
      body: new URLSearchParams(fields).toString(),
    }));
  }

  /** The ledger rows of one issue as `[report, source, by]`. */
  async function ledger(env, issue) {
    return (await all(env.DASHBOARD_DB, "SELECT report_id, source, linked_by FROM report_issue WHERE issue_number = ? ORDER BY report_id", issue))
      .map((r) => [r.report_id, r.source, r.linked_by]);
  }

  it("shows filed, correlated and suggested reports and lets a member attach, detach, move and merge", async () => {
    gh = scriptGitHub([]);
    const env = makeEnv();
    const d1 = env.DASHBOARD_DB;
    for (const id of [ID1, ID2, ID3, ID4]) await seed(env, id, NOW - 3_600_000 - Number(id.slice(-1)) * 60_000);
    await openIssue(env, 400, [ID1]);
    assert.deepEqual([...(await linkBySignature(d1, [ID2], NOW - 60_000))], [[ID2, 400]]);
    const cookie = await signedIn(env, "member");

    const page = await call({ ...env, DASHBOARD_DB: limitedD1(d1, 50) }, new Request(`${ORIGIN}/issues/400`, { headers: { cookie } }));
    assert.equal(page.status, 200);
    let body = await page.text();
    assert.match(body, /<h1>#400 \[diagnostics\] issue 400<\/h1>/);
    assert.match(body, new RegExp(`href="/reports/${ID1}">7c1e4a90</a>[^]*?<td class="fine">filed<`), "the filed report with its source");
    assert.match(body, new RegExp(`href="/reports/${ID2}">d2b80f13</a>[^]*?<td class="fine">signature<`), "the signature link");
    assert.match(body, new RegExp(`name="report" value="${ID3}"[^]*?>Attach<`), "an unlinked report with the signature is suggested");
    assert.match(body, /summary comment on GitHub is out of date \(never written; 1 ledger change since\)/, "the signature link is pending on GitHub");
    assert.ok(!body.includes("Update GitHub now"), "members do not see the owner button");
    assert.ok(!body.includes("<b>stalls"));
    assert.equal((await call(env, new Request(`${ORIGIN}/issues/999`, { headers: { cookie } }))).status, 404);

    // Origin check, malformed form, unknown issue and unknown report.
    assert.equal((await correct(env, cookie, { action: "attach", issue: "400", report: ID3 }, {})).status, 403);
    assert.equal((await correct(env, cookie, { action: "attach", issue: "400" })).status, 400);
    assert.equal((await correct(env, cookie, { action: "attach", issue: "999", report: ID3 })).status, 404);
    assert.equal((await correct(env, cookie, { action: "attach", issue: "400", report: "d2b80f13-0000-4000-8000-00000000000f" })).status, 404);
    assert.deepEqual(await ledger(env, 400), [[ID1, "filed", null], [ID2, "signature", "ingest"]], "refusals write nothing");

    // Attach a suggestion, by name, and return to the page the form named.
    let res = await correct(env, cookie, { action: "attach", issue: "400", report: ID3, back: `/reports/${ID3}` });
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("location"), `/reports/${ID3}`);
    res = await correct(env, cookie, { action: "detach", issue: "400", report: ID4, back: "https://evil.example/" });
    assert.equal(res.headers.get("location"), "/issues/400", "an outside return page is dropped");
    assert.deepEqual(await ledger(env, 400), [[ID1, "filed", null], [ID2, "signature", "ingest"], [ID3, "manual", "octo"], [ID4, "excluded", "octo"]]);
    assert.deepEqual([...(await linkBySignature(d1, [ID4], NOW))], [], "a dismissed report is not linked by signature again");
    body = await (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie } }))).text();
    assert.ok(!body.includes(`name="report" value="${ID4}"`) || /Attach again/.test(body), "a dismissed report leaves the suggestions");
    assert.match(body, /attached by octo on Oct 23/);

    // Detach a filed report; the next sync of the body must not relink it.
    assert.equal((await correct(env, cookie, { action: "detach", issue: "400", report: ID1 })).status, 303);
    await openIssue(env, 400, [ID1]);
    assert.deepEqual((await ledger(env, 400)).find(([id]) => id === ID1), [ID1, "excluded", "octo"]);
    assert.equal(Number((await listIssues(d1, "open", 0, 10)).find((i) => i.number === 400).reports), 2, "the board counts only linked reports");
    body = await (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie } }))).text();
    assert.match(body, /2 reports detached from this issue/);
    assert.match(body, />Attach again</);

    // Move one report to another issue, then merge that issue back.
    await openIssue(env, 403, []);
    res = await correct(env, cookie, { action: "move", issue: "400", report: ID2, to: "403" });
    assert.equal(res.headers.get("location"), "/issues/400");
    assert.deepEqual(await ledger(env, 403), [[ID2, "manual", "octo"]]);
    assert.deepEqual((await ledger(env, 400)).find(([id]) => id === ID2), [ID2, "excluded", "octo"]);
    assert.equal((await correct(env, cookie, { action: "move", issue: "400", report: ID3, to: "400" })).status, 400, "a move onto itself is refused");
    res = await correct(env, cookie, { action: "merge", issue: "403", to: "400" });
    assert.equal(res.headers.get("location"), "/issues/400", "a merge lands on the surviving issue");
    assert.deepEqual(await ledger(env, 403), [[ID2, "excluded", "octo"]]);
    assert.deepEqual((await ledger(env, 400)).find(([id]) => id === ID2), [ID2, "manual", "octo"], "a merge by hand replaces an earlier exclusion");
    assert.deepEqual(
      (await all(d1, "SELECT action, report_id FROM access_log WHERE action IN ('attach', 'detach', 'move', 'merge') ORDER BY id")).map((r) => [r.action, r.report_id]),
      [["attach", ID3], ["detach", ID4], ["detach", ID1], ["move", ID2], ["merge", null]],
    );

    // The report page offers a picker to an untracked report and detach on a tracked one.
    const untracked = await (await call(env, new Request(`${ORIGIN}/reports/${ID4}`, { headers: { cookie } }))).text();
    assert.match(untracked, /Attach to issue<\/label>\s*<select[^>]*name="issue"/, "the picker chooses the issue itself");
    assert.ok(!/name="issue" value=/.test(untracked.slice(untracked.indexOf("Attach to issue"))), "no hidden issue competes with the picker");
    assert.match(untracked, /<option value="403">#403/);
    res = await correct(env, cookie, { action: "attach", report: ID4, issue: "403", back: `/reports/${ID4}` });
    assert.equal(res.headers.get("location"), `/reports/${ID4}`);
    assert.deepEqual((await ledger(env, 403)).find(([id]) => id === ID4), [ID4, "manual", "octo"], "the picked issue gets the report");
    // The holding issue for untriaged reports takes no report by hand.
    await storeIssue(d1, { number: 410, title: "[diagnostics] untriaged reports", state: "open", body: "<!-- bookclerk-diagnostics: triage fallback -->\n### Report IDs\n", created_at: new Date(NOW).toISOString(), updated_at: new Date(NOW).toISOString() });
    assert.equal((await correct(env, cookie, { action: "attach", issue: "410", report: ID4 })).status, 400);
    assert.equal((await correct(env, cookie, { action: "merge", issue: "403", to: "410" })).status, 400);
    assert.equal((await correct(env, cookie, { action: "detach", issue: "410", report: ID4 })).status, 303, "detaching from it is allowed");
    const tracked = await (await call(env, new Request(`${ORIGIN}/reports/${ID3}`, { headers: { cookie } }))).text();
    assert.match(tracked, /href="\/issues\/400">#400 \[diagnostics\] issue 400<\/a>[^]*?>Detach</);
  });

  it("lets an explicit attach, move or merge take a report from the issues that counted it", async () => {
    gh = scriptGitHub([]);
    const KEY = "r".repeat(32);
    const env = makeEnv({ REPORT_API_KEY: KEY });
    const d1 = env.DASHBOARD_DB;
    for (const id of [ID1, ID2, ID3]) await seed(env, id, NOW - 3_600_000);
    // #400 (older, closed) lists the report; triage later filed it on #403 too.
    await openIssue(env, 400, [ID1, ID2]);
    await run(d1, "UPDATE issues SET state = 'closed' WHERE number = 400");
    await openIssue(env, 403, [ID1]);
    await openIssue(env, 405, []);
    /** Reports counted per issue, as the summaries route hands them out. */
    const counted = async () => {
      const handed = await (await worker.fetch(new Request(`${ORIGIN}/issues/summaries`, { headers: { authorization: `Bearer ${KEY}` } }), env, makeCtx())).json();
      return Object.fromEntries(handed.issues.map((i) => [i.issue, i.reports]));
    };
    assert.deepEqual(await counted(), { 400: 2 }, "the oldest issue tracks a report both list");

    // A member's attach on #403 takes it, although triage had filed it there already.
    const cookie = await signedIn(env, "member");
    assert.equal((await correct(env, cookie, { action: "attach", issue: "403", report: ID1 })).status, 303);
    assert.deepEqual(await ledger(env, 400), [[ID1, "excluded", "octo"], [ID2, "filed", null]]);
    assert.deepEqual(await ledger(env, 403), [[ID1, "filed", null]], "the filed record stays");
    assert.deepEqual(await counted(), { 400: 1, 403: 1 });
    assert.deepEqual([...(await linkBySignature(d1, [ID1], NOW))], [], "the taken report is not linked back by signature");
    await openIssue(env, 400, [ID1, ID2]);
    assert.deepEqual((await ledger(env, 400)).find(([id]) => id === ID1), [ID1, "excluded", "octo"], "a sync of #400's description does not relink it");

    // A move takes the report from every other issue, not only the one it leaves.
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 405, 'manual', 'octo', ?)", ID2, NOW);
    assert.equal((await correct(env, cookie, { action: "move", issue: "405", report: ID2, to: "403" })).status, 303);
    assert.deepEqual(await ledger(env, 400), [[ID1, "excluded", "octo"], [ID2, "excluded", "octo"]]);
    assert.deepEqual(await ledger(env, 405), [[ID2, "excluded", "octo"]]);
    assert.deepEqual(await ledger(env, 403), [[ID1, "filed", null], [ID2, "manual", "octo"]]);

    // A merge takes every report the merged issue linked from every other
    // issue, including one the chosen issue already listed itself, and
    // leaves the merged issue with nothing.
    await seed(env, ID4, NOW - 3_600_000);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 405, 'manual', 'octo', ?)", ID3, NOW);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 400, 'manual', 'octo', ?)", ID3, NOW - 1);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 405, 'signature', 'ingest', ?)", ID1, NOW);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 400, 'filed')", ID4);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source) VALUES (?, 403, 'filed')", ID4);
    await run(d1, "INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, 405, 'signature', 'ingest', ?)", ID4, NOW);
    assert.deepEqual(await counted(), { 400: 2, 403: 2 });
    assert.equal((await correct(env, cookie, { action: "merge", issue: "405", to: "403" })).status, 303);
    assert.deepEqual(await ledger(env, 405), [[ID1, "excluded", "octo"], [ID2, "excluded", "octo"], [ID3, "excluded", "octo"], [ID4, "excluded", "octo"]]);
    assert.deepEqual((await ledger(env, 400)).filter(([id]) => id === ID3 || id === ID4), [[ID3, "excluded", "octo"], [ID4, "excluded", "octo"]], "#400 counted both, so the merge takes them");
    assert.deepEqual((await ledger(env, 403)).map(([id, source]) => [id, source]), [[ID1, "filed"], [ID2, "manual"], [ID3, "manual"], [ID4, "filed"]]);
    assert.deepEqual(await counted(), { 403: 4 }, "#400 tracks nothing now (its summary, never written here, is not handed out)");

    // A move from an issue whose row the sync already dropped still records the exclusion there.
    await run(d1, "DELETE FROM report_issue WHERE report_id = ? AND issue_number = 403", ID2);
    assert.equal((await correct(env, cookie, { action: "move", issue: "403", report: ID2, to: "405" })).status, 303);
    assert.deepEqual((await ledger(env, 403)).find(([id]) => id === ID2), [ID2, "excluded", "octo"]);
    assert.deepEqual(await ledger(env, 405), [[ID1, "excluded", "octo"], [ID2, "manual", "octo"], [ID3, "excluded", "octo"], [ID4, "excluded", "octo"]]);
  });

  it("clears the pending banner once ingest records the summary", async () => {
    gh = scriptGitHub([]);
    const KEY = "r".repeat(32);
    const env = makeEnv({ REPORT_API_KEY: KEY });
    await seed(env, ID1);
    await seed(env, ID2);
    await openIssue(env, 400, [ID1]);
    await linkBySignature(env.DASHBOARD_DB, [ID2], NOW - 60_000);
    const cookie = await signedIn(env, "member");
    assert.match(await (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie } }))).text(), /out of date \(never written; 1 ledger change since\)/);
    const handed = await (await worker.fetch(new Request(`${ORIGIN}/issues/summaries`, { headers: { authorization: `Bearer ${KEY}` } }), env, makeCtx())).json();
    const fingerprint = handed.issues.find((i) => i.issue === 400).fingerprint;
    const done = await worker.fetch(
      new Request(`${ORIGIN}/issues/summary`, { method: "POST", headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" }, body: JSON.stringify({ issue: 400, fingerprint }) }),
      env,
      makeCtx(),
    );
    assert.equal(done.status, 200);
    assert.ok(Number((await first(env.DASHBOARD_DB, "SELECT summary_sent_at AS at FROM issues WHERE number = 400")).at) >= NOW);
    const after = await (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie } }), makeB2(), makeCtx(), NOW + 1000)).text();
    assert.ok(!after.includes("out of date"), "nothing pending once the summary is recorded");
    // A dismissed suggestion changes nothing the comment shows, so GitHub
    // stays current; detaching a tracked report makes it out of date again.
    const ID3 = "d2b80f13-0000-4000-8000-000000000003";
    await seed(env, ID3);
    // The receipt was stamped with the real clock, so later changes are too.
    const later = Date.now() + 60_000;
    const fresh = (await createSession(env.DASHBOARD_DB, { userId: 7, login: "octo", role: "member" }, later)).split(";")[0];
    const post = (fields) => call(env, new Request(`${ORIGIN}/issues/link`, { method: "POST", headers: { cookie: fresh, origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" }, body: new URLSearchParams(fields).toString() }), makeB2(), makeCtx(), later);
    const page = async () => (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie: fresh } }), makeB2(), makeCtx(), later)).text();
    assert.equal((await post({ action: "detach", issue: "400", report: ID3 })).status, 303);
    assert.ok(!(await page()).includes("out of date"));
    assert.equal((await post({ action: "detach", issue: "400", report: ID2 })).status, 303);
    assert.match(await page(), /out of date \(last written [^)]*; 2 ledger changes since\)/);
  });

  it("lets an owner write the summary comment with an issues-only token", async () => {
    const env = makeEnv();
    await seed(env, ID1);
    await seed(env, ID2);
    await openIssue(env, 400, [ID1]);
    await linkBySignature(env.DASHBOARD_DB, [ID2], NOW - 60_000);
    const MARKER = "<!-- bookclerk-diagnostics: summary -->";
    let comments = [{ id: 11, user: { login: "octo", type: "User" }, body: "me too" }];
    let writeStatus = 200;
    let writeBody = "{}";
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => Response.json(comments)],
      ["POST https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => new Response(writeBody, { status: writeStatus })],
      ["PATCH https://api.github.com/repos/Bookclerk/bookclerk/issues/comments/", () => new Response(writeBody, { status: writeStatus })],
    ]);
    const member = await signedIn(env, "member");
    const press = (cookie, now, headers = { origin: ORIGIN }, body = "issue=400&back=%2Fissues%2F400") => call(env, new Request(`${ORIGIN}/admin/update-issue`, {
      method: "POST",
      headers: { cookie, "content-type": "application/x-www-form-urlencoded", ...headers },
      body,
    }), makeB2(), makeCtx(), now);
    assert.equal((await press(member, NOW)).status, 403);
    const owner = await signedIn(env, "admin");
    assert.equal((await press(owner, NOW, {})).status, 403, "needs the dashboard's own form");
    assert.equal((await press(owner, NOW, { origin: ORIGIN }, "back=%2Fissues%2F400")).status, 400, "names an issue");
    const page = async (n = 400) => (await call(env, new Request(`${ORIGIN}/issues/${n}`, { headers: { cookie: owner } }), makeB2(), makeCtx(), NOW + 1000)).text();
    assert.match(await page(), /out of date[^]*?Update GitHub now/);

    // The first write posts the comment: no bot comment carries the marker.
    const res = await press(owner, NOW);
    assert.equal(res.status, 303);
    assert.equal(res.headers.get("location"), "/issues/400");
    const mint = gh.calls.find((c) => c.url.endsWith("/access_tokens"));
    assert.deepEqual(mint.body, { repositories: ["bookclerk"], permissions: { issues: "write" } }, "the token can only write issues");
    const posted = gh.calls.find((c) => c.method === "POST" && c.url.endsWith("/issues/400/comments"));
    assert.equal(posted.headers.Authorization, "Bearer ghs_installation");
    assert.ok(posted.body.body.startsWith(`${MARKER}\n**Diagnostics summary** · 2 reports from 1 installation`), posted.body.body);
    assert.match(posted.body.body, /- Linked: 1 listed in this issue, 1 matched by error signature, 0 added by triage, 0 attached by members/);
    assert.ok(posted.body.body.includes(`[dashboard](${ORIGIN}/reports/${ID1})`), "report links point at this dashboard");
    const sent = await first(env.DASHBOARD_DB, "SELECT summary_sent AS fp, summary_sent_at AS at FROM issues WHERE number = 400");
    assert.match(sent.fp, /^[0-9a-f]{64}$/);
    assert.equal(Number(sent.at), NOW);
    assert.equal((await first(env.DASHBOARD_DB, "SELECT action FROM access_log ORDER BY id DESC LIMIT 1")).action, "update-issue");
    let shown = await page();
    assert.match(shown, /The summary comment was written to GitHub/);
    assert.ok(!shown.includes("out of date"), "the banner clears once the comment is written");
    assert.ok(!shown.includes("Update GitHub now"), "nothing to update");
    // The same figures again reach no GitHub write.
    assert.equal((await press(owner, NOW + 2000)).status, 303);
    assert.equal(gh.calls.filter((c) => c.method === "POST" && c.url.endsWith("/issues/400/comments")).length, 1);
    assert.match(await page(), /already shows the current figures/);

    // A later change edits the existing bot comment in place.
    comments = [{ id: 11, user: { login: "octo", type: "User" }, body: `${MARKER} not a bot` }, { id: 12, user: { login: "github-actions[bot]", type: "Bot" }, body: `${MARKER}\nold` }];
    await linkBySignature(env.DASHBOARD_DB, [ID2], NOW + 3000); // no change: already tracked
    await seed(env, "d2b80f13-0000-4000-8000-000000000005", NOW + 3000);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000005"], NOW + 3000);
    assert.equal((await press(owner, NOW + 4000)).status, 303);
    const patched = gh.calls.find((c) => c.method === "PATCH");
    assert.ok(patched.url.endsWith("/issues/comments/12"), "the bot's marker comment, not the member's");
    assert.match(patched.body.body, /3 reports/);
    assert.match(await page(), /summary comment on GitHub was updated/);
    assert.equal(gh.calls.filter((c) => c.method === "POST" && c.url.endsWith("/issues/400/comments")).length, 1);

    // The comment was deleted between the listing and the edit: a new one
    // is posted rather than the issue given up on.
    await seed(env, "d2b80f13-0000-4000-8000-000000000008", NOW + 4500);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000008"], NOW + 4500);
    let patchGone = true;
    writeStatus = 201;
    gh.restore();
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => Response.json(comments)],
      ["PATCH https://api.github.com/repos/Bookclerk/bookclerk/issues/comments/", () => (patchGone ? new Response("{}", { status: 404 }) : new Response(writeBody, { status: writeStatus }))],
      ["POST https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => new Response(writeBody, { status: writeStatus })],
    ]);
    assert.equal((await press(owner, NOW + 4600)).status, 303);
    assert.equal(gh.calls.filter((c) => c.method === "PATCH").length, 1);
    assert.equal(gh.calls.filter((c) => c.method === "POST" && c.url.endsWith("/issues/400/comments")).length, 1, "posted once more");
    assert.match(await page(), /summary comment was written to GitHub/);
    assert.equal(Number((await first(env.DASHBOARD_DB, "SELECT summary_sent_at AS at FROM issues WHERE number = 400")).at), NOW + 4600);
    patchGone = false;

    // A locked issue is a refusal for good: recorded, reported, not retried.
    await seed(env, "d2b80f13-0000-4000-8000-000000000006", NOW + 5000);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000006"], NOW + 5000);
    writeStatus = 403;
    writeBody = JSON.stringify({ message: "Unable to create comment because issue is locked." });
    assert.equal((await press(owner, NOW + 6000)).status, 303);
    assert.match(await page(), /The issue is locked/);
    assert.equal(Number((await first(env.DASHBOARD_DB, "SELECT summary_sent_at AS at FROM issues WHERE number = 400")).at), NOW + 6000);

    // A rate limit is not: nothing is recorded, so the scheduled run retries.
    await seed(env, "d2b80f13-0000-4000-8000-000000000007", NOW + 7000);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000007"], NOW + 7000);
    writeBody = JSON.stringify({ message: "API rate limit exceeded" });
    assert.equal((await press(owner, NOW + 8000)).status, 303);
    assert.match(await page(), /GitHub did not take the comment \(comment edit failed: HTTP 403\)/);
    assert.equal(Number((await first(env.DASHBOARD_DB, "SELECT summary_sent_at AS at FROM issues WHERE number = 400")).at), NOW + 6000);
    assert.match(await page(), /out of date/);
    // The note belongs to #400 only, and fades after ten minutes.
    await openIssue(env, 403, [ID2]);
    assert.ok(!(await page(403)).includes("did not take the comment"));
    const later = await (await call(env, new Request(`${ORIGIN}/issues/400`, { headers: { cookie: owner } }), makeB2(), makeCtx(), NOW + 8000 + 11 * 60_000)).text();
    assert.ok(!later.includes("did not take the comment"), "an old note is not shown");

    // A failed listing is left for the scheduled run, like a rate limit.
    gh.restore();
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => new Response("{}", { status: 404 })],
    ]);
    assert.equal((await press(owner, NOW + 9000)).status, 303);
    assert.match(await page(), /comment listing failed: HTTP 404/);
    assert.equal(Number((await first(env.DASHBOARD_DB, "SELECT summary_sent_at AS at FROM issues WHERE number = 400")).at), NOW + 6000);
    // While the ingest run holds the issue's summary (handed out, not yet
    // acknowledged) the press waits; the acknowledgement frees it. (The
    // hand-out and the acknowledgement are stamped with the real clock, so
    // this comes last.)
    const KEY = "r".repeat(32);
    const keyed = makeEnv({ REPORT_API_KEY: KEY });
    keyed.DASHBOARD_DB = env.DASHBOARD_DB;
    const handed = await (await worker.fetch(new Request(`${ORIGIN}/issues/summaries`, { headers: { authorization: `Bearer ${KEY}` } }), keyed, makeCtx())).json();
    const mine = handed.issues.find((i) => i.issue === 400);
    assert.ok(mine, "the run is handed the pending summary");
    gh.restore();
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", () => Response.json(comments)],
      ["PATCH https://api.github.com/repos/Bookclerk/bookclerk/issues/comments/", () => Response.json({})],
    ]);
    assert.equal((await press(owner, NOW + 9500)).status, 303);
    assert.match(await page(), /ingest run \(or another owner\) is writing this comment now/);
    assert.equal(gh.calls.length, 0, "GitHub is not called while the run holds it");
    const ack = await worker.fetch(
      new Request(`${ORIGIN}/issues/summary`, { method: "POST", headers: { authorization: `Bearer ${KEY}`, "content-type": "application/json" }, body: JSON.stringify({ issue: 400, fingerprint: mine.fingerprint }) }),
      keyed,
      makeCtx(),
    );
    assert.equal(ack.status, 200);
    await seed(env, "d2b80f13-0000-4000-8000-000000000009", NOW + 9600);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000009"], NOW + 9600);
    assert.equal((await press(owner, NOW + 9700)).status, 303);
    assert.match(await page(), /summary comment on GitHub was updated/);
    // A press in flight keeps the issue out of the run's hand-out.
    await env.DASHBOARD_DB.prepare("UPDATE issues SET summary_lock_until = ? WHERE number = 400").bind(Date.now() + 60_000).run();
    await seed(env, "d2b80f13-0000-4000-8000-000000000010", NOW + 9800);
    await linkBySignature(env.DASHBOARD_DB, ["d2b80f13-0000-4000-8000-000000000010"], NOW + 9800);
    const held = await (await worker.fetch(new Request(`${ORIGIN}/issues/summaries`, { headers: { authorization: `Bearer ${KEY}` } }), keyed, makeCtx())).json();
    assert.ok(!held.issues.some((i) => i.issue === 400), "a held issue is not handed out");
    await env.DASHBOARD_DB.prepare("UPDATE issues SET summary_lock_until = NULL WHERE number = 400").run();

  });

  it("finds the summary comment past the first page of comments", async () => {
    const env = makeEnv();
    await seed(env, ID1);
    await openIssue(env, 400, [ID1]);
    const MARKER = "<!-- bookclerk-diagnostics: summary -->";
    const human = (i) => ({ id: 1000 + i, user: { login: "octo", type: "User" }, body: `comment ${i}` });
    const pages = {
      1: Array.from({ length: 100 }, (_, i) => human(i)),
      2: Array.from({ length: 100 }, (_, i) => human(100 + i)),
      3: [human(200), { id: 7, user: { login: "github-actions[bot]", type: "Bot" }, body: `${MARKER}\nold` }],
    };
    gh = scriptGitHub([
      ...appRoutes(null),
      ["GET https://api.github.com/repos/Bookclerk/bookclerk/issues/400/comments", ({ url }) => Response.json(pages[new URL(url).searchParams.get("page")] ?? [])],
      ["PATCH https://api.github.com/repos/Bookclerk/bookclerk/issues/comments/7", () => Response.json({})],
    ]);
    const owner = await signedIn(env, "admin");
    const res = await call(env, new Request(`${ORIGIN}/admin/update-issue`, {
      method: "POST",
      headers: { cookie: owner, origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" },
      body: "issue=400&back=%2Fissues%2F400",
    }));
    assert.equal(res.status, 303);
    const listings = gh.calls.filter((c) => c.method === "GET" && c.url.includes("/issues/400/comments"));
    assert.deepEqual(listings.map((c) => new URL(c.url).searchParams.get("page")), ["1", "2", "3"]);
    assert.equal(gh.calls.filter((c) => c.method === "PATCH").length, 1);
    assert.equal(gh.calls.filter((c) => c.method === "POST" && c.url.endsWith("/comments")).length, 0, "nothing posted");
  });
});
