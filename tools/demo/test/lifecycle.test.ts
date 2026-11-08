// Session start and teardown against a scripted bookclerkd: the visitor's
// Owner is created with the operator token and its claim ticket redeemed,
// and teardown signs out every linked store account even when some fail.

import assert from "node:assert/strict";
import { describe, it, vi } from "vitest";
import {
  enableDemoPlugins,
  enableStorefronts,
  OPTIONAL_PLUGINS,
  STOREFRONTS,
  signInVisitor,
  VISITOR_PASSWORD,
  signOutStores,
  waitForHealth,
  prepareShelves,
  SHELVES_WAIT,
  waitForShelves,
} from "../src/lifecycle.ts";
import { type Sent, sent } from "../test-support/requests.ts";

/**
 * A scripted bookclerkd: routes map `METHOD path` to a handler.
 *
 * @param routes - Handlers.
 */
function scripted(routes: Record<string, (init: RequestInit) => Response>) {
  const seen: Sent[] = [];
  const call = async (path: string, init: RequestInit = {}): Promise<Response> => {
    const key = `${init.method ?? "GET"} ${path}`;
    seen.push(sent(key, init));
    const h = routes[key];
    if (!h) return new Response("not found", { status: 404 });
    return h(init);
  };
  return { call, seen };
}

describe("signInVisitor", () => {
  const redeemed = () => {
    const h = new Headers();
    h.append(
      "Set-Cookie",
      `bookclerk_portal_session=${"a".repeat(64)}; Path=/; HttpOnly; SameSite=Lax`,
    );
    return new Response('{"ok":true}', { headers: h });
  };

  it("makes the visitor the Owner and redeems its ticket", async () => {
    const ticket = "0b6e2d1c-3c54-4f0e-9c1e-1b0e7b8a9d10";
    const { call, seen } = scripted({
      "POST /api/auth/bootstrap": () =>
        Response.json({ ok: true, user_id: 1, claim_ticket: ticket }),
      "POST /api/portal/redeem": redeemed,
    });
    assert.equal(await signInVisitor(call, "op-token"), "a".repeat(64));
    assert.equal(seen.length, 2);
    assert.equal(seen[0]!.init.headers.Authorization, "Bearer op-token");
    const boot = JSON.parse(seen[0]!.init.body);
    assert.equal(boot.login_name, "demo");
    assert.equal(boot.display_name, "Demo visitor");
    assert.equal(boot.password, VISITOR_PASSWORD);
    const redeem = JSON.parse(seen[1]!.init.body);
    assert.equal(redeem.ticket, ticket);
    assert.match(redeem.nonce, /^[0-9a-f]{64}$/);
    assert.equal(seen[1]!.init.headers.Authorization, undefined);
  });

  it("fails when bootstrap or redeem goes wrong", async () => {
    await assert.rejects(
      signInVisitor(
        scripted({ "POST /api/auth/bootstrap": () => new Response("", { status: 409 }) }).call,
        "t",
      ),
      /bootstrap answered 409/,
    );
    await assert.rejects(
      signInVisitor(
        scripted({
          "POST /api/auth/bootstrap": () => Response.json({ ok: true, claim_ticket: "short" }),
        }).call,
        "t",
      ),
      /bootstrap answered 200/,
    );
    await assert.rejects(
      signInVisitor(
        scripted({
          "POST /api/auth/bootstrap": () =>
            Response.json({ ok: true, claim_ticket: "x".repeat(36) }),
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
      "POST /api/portal/sources/audible/connections/amzn1.account.X/revoke": () =>
        Response.json({ ok: true }),
      "POST /api/portal/sources/libro/connections/me%40example.test/revoke": () =>
        new Response("", { status: 500 }),
    });
    assert.deepEqual(await signOutStores(call, "s".repeat(64), 1000, "https://demo.example.test"), {
      revoked: 1,
      failed: 1,
    });
    assert.equal(seen.length, 3);
    for (const s of seen)
      assert.equal(s.init.headers.Cookie, `bookclerk_portal_session=${"s".repeat(64)}`);
    for (const s of seen.slice(1)) assert.equal(s.init.headers.Origin, "https://demo.example.test");
  });

  it("reports one failure when the account list cannot be read", async () => {
    const { call } = scripted({
      "GET /api/portal/connections": () => new Response("nope", { status: 500 }),
    });
    assert.deepEqual(await signOutStores(call, "s".repeat(64), 1000, "https://demo.example.test"), {
      revoked: 0,
      failed: 1,
    });
  });
});

describe("waitForHealth", () => {
  it("returns once bookclerkd answers 200", async () => {
    let n = 0;
    await waitForHealth(async () => new Response("", { status: ++n < 3 ? 503 : 200 }), {
      timeoutMs: 1000,
      intervalMs: 1,
    });
    assert.equal(n, 3);
  });

  it("gives up after the timeout", async () => {
    await assert.rejects(
      waitForHealth(async () => new Response("", { status: 503 }), {
        timeoutMs: 20,
        intervalMs: 5,
      }),
      /did not become healthy: status 503/,
    );
  });
});

describe("waitForShelves", () => {
  const session = "s".repeat(64);

  it("reads the status as the visitor until the shelves are ready", async () => {
    let n = 0;
    const { call, seen } = scripted({
      "GET /api/discover/status": () =>
        ++n === 1
          ? new Response("busy", { status: 503 })
          : Response.json({ subject: "user:1", ready: n >= 3 }),
    });
    assert.equal(await waitForShelves(call, session, { timeoutMs: 1000, intervalMs: 1 }), true);
    assert.equal(n, 3);
    assert.ok(seen.every((s) => s.init.headers.Cookie === `bookclerk_portal_session=${session}`));
  });

  it("gives up at the deadline without throwing", async () => {
    const { call } = scripted({
      "GET /api/discover/status": () => Response.json({ subject: "user:1", ready: false }),
    });
    assert.equal(await waitForShelves(call, session, { timeoutMs: 20, intervalMs: 5 }), false);
  });

  it("treats a failed or unreadable read as not ready", async () => {
    let n = 0;
    const call = async (): Promise<Response> => {
      n += 1;
      if (n === 1) throw new Error("connection reset");
      if (n === 2) return new Response("not json");
      return Response.json({ ready: true });
    };
    assert.equal(await waitForShelves(call, session, { timeoutMs: 1000, intervalMs: 1 }), true);
    assert.equal(n, 3);
  });

  it("does not read at all with no time left", async () => {
    let n = 0;
    const call = async (): Promise<Response> => {
      n += 1;
      return Response.json({ ready: true });
    };
    assert.equal(await waitForShelves(call, session, { timeoutMs: 0, intervalMs: 1 }), false);
    assert.equal(n, 0);
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
        "GET /api/config/plugin-instances",
        "POST /api/plugins/audible/consent",
        "POST /api/plugins/libro/consent",
        "GET /api/config/plugin-instances",
        "PUT /api/config/plugin-instances/i-audible/deployments/node-1",
        "PUT /api/config/plugin-instances/i-libro/deployments/node-1",
      ],
    );
    for (const s of seen) assert.equal(s.init.headers.Authorization, "Bearer op");
    assert.deepEqual(JSON.parse(seen[1]!.init.body), { approve: true });
    assert.deepEqual(JSON.parse(seen[4]!.init.body), { desired: "present" });
  });

  it("leaves plugins already deployed on this node alone", async () => {
    const here = { node_id: "node-1", desired: "present", revision: 2 };
    const listing = {
      node_id: "node-1",
      instances: [
        {
          plugin_instance_id: "i-audible",
          alias: "audible",
          is_default: true,
          deployments: [here],
        },
        {
          plugin_instance_id: "i-libro",
          alias: "libro",
          is_default: true,
          // Present on another node, or absent here: still to do.
          deployments: [
            { node_id: "node-2", desired: "present" },
            { node_id: "node-1", desired: "absent" },
          ],
        },
      ],
    };
    const { call, seen } = scripted({
      "GET /api/config/plugin-instances": () => Response.json(listing),
      "POST /api/plugins/libro/consent": () => Response.json({}),
      "PUT /api/config/plugin-instances/i-libro/deployments/node-1": () => Response.json({}),
    });
    await enableStorefronts(call, "op", ["audible", "libro"]);
    assert.deepEqual(
      seen.map((s) => s.key),
      [
        "GET /api/config/plugin-instances",
        "POST /api/plugins/libro/consent",
        "GET /api/config/plugin-instances",
        "PUT /api/config/plugin-instances/i-libro/deployments/node-1",
      ],
    );
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
    await assert.rejects(
      enableStorefronts(scripted({}).call, "op", ["audible"]),
      /plugin instances answered 404/,
    );
    await assert.rejects(
      enableStorefronts(
        scripted({ "GET /api/config/plugin-instances": () => Response.json(instances) }).call,
        "op",
        ["audible"],
      ),
      /consent for audible answered 404/,
    );
  });
});

describe("enableDemoPlugins", () => {
  const all = [...STOREFRONTS, ...OPTIONAL_PLUGINS];
  const instances = () =>
    Response.json({
      node_id: "node-1",
      instances: all.map((id) => ({ plugin_instance_id: `i-${id}`, alias: id, is_default: true })),
    });
  const routes = (ids: string[]) => {
    const r: Record<string, (init: RequestInit) => Response> = {
      "GET /api/config/plugin-instances": instances,
    };
    for (const id of ids) {
      r[`POST /api/plugins/${id}/consent`] = () => Response.json({});
      r[`PUT /api/config/plugin-instances/i-${id}/deployments/node-1`] = () => Response.json({});
    }
    return r;
  };

  it("enables the storefronts and then covers", async () => {
    assert.deepEqual(OPTIONAL_PLUGINS, ["covers"]);
    const warnings: string[] = [];
    const { call, seen } = scripted(routes(all));
    await enableDemoPlugins(call, "op", (m) => warnings.push(m));
    const keys = seen.map((s) => s.key);
    assert.ok(keys.includes("POST /api/plugins/audible/consent"));
    assert.ok(keys.includes("PUT /api/config/plugin-instances/i-covers/deployments/node-1"));
    assert.deepEqual(warnings, []);
  });

  it("keeps the session when covers can't be enabled", async () => {
    const warnings: string[] = [];
    await enableDemoPlugins(scripted(routes(STOREFRONTS)).call, "op", (m) => warnings.push(m));
    assert.deepEqual(warnings, ["demo: covers not enabled: consent for covers answered 404"]);
  });

  it("only reads the list when a baked image has every plugin deployed", async () => {
    const baked = () =>
      Response.json({
        node_id: "node-1",
        instances: all.map((id) => ({
          plugin_instance_id: `i-${id}`,
          alias: id,
          is_default: true,
          deployments: [{ node_id: "node-1", desired: "present", revision: 2 }],
        })),
      });
    const warnings: string[] = [];
    const { call, seen } = scripted({ "GET /api/config/plugin-instances": baked });
    await enableDemoPlugins(call, "op", (m) => warnings.push(m));
    assert.deepEqual(
      seen.map((s) => s.key),
      ["GET /api/config/plugin-instances"],
    );
    assert.deepEqual(warnings, []);
  });

  it("still fails when a storefront can't be enabled", async () => {
    await assert.rejects(
      enableDemoPlugins(scripted(routes(["libro"])).call, "op", () => {}),
      /consent for audible answered 404/,
    );
  });
});

describe("prepareShelves", () => {
  const session = "s".repeat(64);

  it("waits up to 25 seconds, a second apart, by default", () => {
    assert.deepEqual(SHELVES_WAIT, { timeoutMs: 25_000, intervalMs: 1_000 });
  });

  it("returns quietly when the shelves are ready", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const call = async (): Promise<Response> => Response.json({ ready: true });
    assert.equal(await prepareShelves(call, session, "demo-1"), true);
    assert.equal(warn.mock.calls.length, 0);
    warn.mockRestore();
  });

  it("warns and opens anyway when the shelves are late", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const call = async (): Promise<Response> => Response.json({ ready: false });
    assert.equal(
      await prepareShelves(call, session, "demo-1", { timeoutMs: 10, intervalMs: 2 }),
      false,
    );
    assert.equal(warn.mock.calls.length, 1);
    assert.match(String(warn.mock.calls[0]?.[0]), /demo session demo-1 opened before its shelves/);
    warn.mockRestore();
  });
});
