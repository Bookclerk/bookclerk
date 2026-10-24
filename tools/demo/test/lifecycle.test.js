// Session start and teardown against a scripted bookclerkd: the visitor's
// Owner is created with the operator token and its claim ticket redeemed,
// and teardown signs out every linked store account even when some fail.

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { enableStorefronts, signInVisitor, signOutStores, waitForHealth } from "../src/lifecycle.js";

/**
 * A scripted bookclerkd: routes map `METHOD path` to a handler.
 *
 * @param {Record<string, (init: RequestInit) => Response>} routes Handlers.
 */
function scripted(routes) {
  const seen = [];
  const call = async (path, init = {}) => {
    const key = `${init.method ?? "GET"} ${path}`;
    seen.push({ key, init });
    const h = routes[key];
    if (!h) return new Response("not found", { status: 404 });
    return h(init);
  };
  return { call, seen };
}

describe("signInVisitor", () => {
  const redeemed = () => {
    const h = new Headers();
    h.append("Set-Cookie", `bookclerk_portal_session=${"a".repeat(64)}; Path=/; HttpOnly; SameSite=Lax`);
    return new Response('{"ok":true}', { headers: h });
  };

  it("sets up an Owner, creates the visitor as an administrator and redeems its ticket", async () => {
    const ticket = "0b6e2d1c-3c54-4f0e-9c1e-1b0e7b8a9d10";
    const { call, seen } = scripted({
      "POST /api/auth/bootstrap": () => Response.json({ ok: true, claim_ticket: "o".repeat(36) }),
      "POST /api/users": () => Response.json({ ok: true, user: { id: 2 }, claim_ticket: ticket }),
      "POST /api/portal/redeem": redeemed,
    });
    assert.equal(await signInVisitor(call, "op-token"), "a".repeat(64));
    assert.equal(seen[0].init.headers.Authorization, "Bearer op-token");
    const boot = JSON.parse(seen[0].init.body);
    assert.equal(boot.login_name, "demo-owner");
    assert.match(boot.password, /^[0-9a-f]{48}$/);
    assert.equal(seen[1].init.headers.Authorization, "Bearer op-token");
    const user = JSON.parse(seen[1].init.body);
    assert.equal(user.role, "administrator");
    assert.equal(user.login_name, "demo");
    assert.equal(user.mint_invite, true);
    assert.match(user.password, /^[0-9a-f]{48}$/);
    assert.notEqual(user.password, boot.password);
    const redeem = JSON.parse(seen[2].init.body);
    assert.equal(redeem.ticket, ticket);
    assert.match(redeem.nonce, /^[0-9a-f]{64}$/);
    assert.equal(seen[2].init.headers.Authorization, undefined);
  });

  it("fails when bootstrap, user creation or redeem goes wrong", async () => {
    await assert.rejects(
      signInVisitor(scripted({ "POST /api/auth/bootstrap": () => new Response("", { status: 409 }) }).call, "t"),
      /bootstrap answered 409/,
    );
    const booted = { "POST /api/auth/bootstrap": () => Response.json({ ok: true }) };
    await assert.rejects(
      signInVisitor(scripted({ ...booted, "POST /api/users": () => new Response("", { status: 403 }) }).call, "t"),
      /creating the visitor answered 403/,
    );
    await assert.rejects(
      signInVisitor(scripted({ ...booted, "POST /api/users": () => Response.json({ ok: true, claim_ticket: "short" }) }).call, "t"),
      /creating the visitor answered 200/,
    );
    await assert.rejects(
      signInVisitor(
        scripted({
          ...booted,
          "POST /api/users": () => Response.json({ ok: true, claim_ticket: "x".repeat(36) }),
          "POST /api/portal/redeem": () => new Response("{}"),
        }).call,
        "t",
      ),
      /redeem answered 200/,
    );
  });
});

describe("signOutStores", () => {
  it("revokes every linked account and counts failures", async () => {
    const { call, seen } = scripted({
      "GET /api/portal/connections": () =>
        Response.json({
          connections: [
            { source: "audible", account_id: "amzn1.account.X" },
            { source: "libro", account_id: "me@example.test" },
            { source: "audible", account_id: "amzn1.account.X" },
          ],
        }),
      "POST /api/portal/sources/audible/connections/amzn1.account.X/revoke": () => Response.json({ ok: true }),
      "POST /api/portal/sources/libro/connections/me%40example.test/revoke": () => new Response("", { status: 500 }),
    });
    assert.deepEqual(await signOutStores(call, "s".repeat(64), 1000, "https://demo.example.test"), { revoked: 1, failed: 1 });
    assert.equal(seen.length, 3);
    for (const s of seen) assert.equal(s.init.headers.Cookie, `bookclerk_portal_session=${"s".repeat(64)}`);
    for (const s of seen.slice(1)) assert.equal(s.init.headers.Origin, "https://demo.example.test");
  });

  it("reports one failure when the account list cannot be read", async () => {
    const { call } = scripted({ "GET /api/portal/connections": () => new Response("nope", { status: 500 }) });
    assert.deepEqual(await signOutStores(call, "s".repeat(64), 1000, "https://demo.example.test"), { revoked: 0, failed: 1 });
  });
});

describe("waitForHealth", () => {
  it("returns once bookclerkd answers 200", async () => {
    let n = 0;
    await waitForHealth(async () => new Response("", { status: ++n < 3 ? 503 : 200 }), { timeoutMs: 1000, intervalMs: 1 });
    assert.equal(n, 3);
  });

  it("gives up after the timeout", async () => {
    await assert.rejects(
      waitForHealth(async () => new Response("", { status: 503 }), { timeoutMs: 20, intervalMs: 5 }),
      /did not become healthy: status 503/,
    );
  });
});

describe("enableStorefronts", () => {
  const instances = {
    node_id: "node-1",
    instances: [
      { plugin_instance_id: "i-audible", alias: "audible", is_default: true },
      { plugin_instance_id: "i-libro", alias: "libro", is_default: true },
    ],
  };

  it("approves each grant, then deploys each default instance on this node", async () => {
    const { call, seen } = scripted({
      "POST /api/plugins/audible/consent": () => Response.json({}),
      "POST /api/plugins/libro/consent": () => Response.json({}),
      "GET /api/config/plugin-instances": () => Response.json(instances),
      "PUT /api/config/plugin-instances/i-audible/deployments/node-1": () => Response.json({}),
      "PUT /api/config/plugin-instances/i-libro/deployments/node-1": () => Response.json({}),
    });
    await enableStorefronts(call, "op", ["audible", "libro"]);
    assert.deepEqual(
      seen.map((s) => s.key),
      [
        "POST /api/plugins/audible/consent",
        "POST /api/plugins/libro/consent",
        "GET /api/config/plugin-instances",
        "PUT /api/config/plugin-instances/i-audible/deployments/node-1",
        "PUT /api/config/plugin-instances/i-libro/deployments/node-1",
      ],
    );
    for (const s of seen) assert.equal(s.init.headers.Authorization, "Bearer op");
    assert.deepEqual(JSON.parse(seen[0].init.body), { approve: true });
    assert.deepEqual(JSON.parse(seen[3].init.body), { desired: "present" });
  });

  it("fails when a storefront is missing or refused", async () => {
    await assert.rejects(
      enableStorefronts(
        scripted({
          "POST /api/plugins/chirp/consent": () => Response.json({}),
          "GET /api/config/plugin-instances": () => Response.json(instances),
        }).call,
        "op",
        ["chirp"],
      ),
      /no default instance for chirp/,
    );
    await assert.rejects(enableStorefronts(scripted({}).call, "op", ["audible"]), /consent for audible answered 404/);
  });
});
