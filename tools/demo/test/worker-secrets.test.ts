// The deploy-time secrets helper against a scripted Cloudflare API: the
// Turnstile widget is created once, its hostnames kept in step, and the
// session key printed only when the Worker has none.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { newSessionKey, WIDGET_NAME, workerSecrets } from "../scripts/worker-secrets.ts";
import { type Sent, sent } from "../test-support/requests.ts";

const BASE = "https://api.cloudflare.com/client/v4/accounts/acct/challenges/widgets";

/**
 * A scripted Cloudflare API: routes map `METHOD url` to a `result`.
 *
 * @param routes - Results by `METHOD url`.
 */
function scripted(routes: Record<string, unknown>) {
  const seen: Sent[] = [];
  const fetchFn = async (url: string, init: RequestInit = {}): Promise<Response> => {
    const key = `${init.method ?? "GET"} ${url}`;
    seen.push(sent(key, init));
    if (!(key in routes))
      return Response.json({ success: false, errors: [{ code: 10000 }] }, { status: 404 });
    return Response.json({ success: true, result: routes[key] });
  };
  return { fetchFn, seen };
}

const ENV = {
  CLOUDFLARE_API_TOKEN: "tok",
  CLOUDFLARE_ACCOUNT_ID: "acct",
  DEMO_HOST: "demo.example.test",
};
const KEYS = { sitekey: "0x4AAA", secret: "0x4BBB" };

describe("workerSecrets", () => {
  it("creates the widget for the host on the first run", async () => {
    const { fetchFn, seen } = scripted({
      [`GET ${BASE}?per_page=100`]: [{ name: "other", sitekey: "x" }],
      [`POST ${BASE}`]: { name: WIDGET_NAME, ...KEYS },
    });
    assert.deepEqual(await workerSecrets(ENV, fetchFn), {
      TURNSTILE_SITE_KEY: "0x4AAA",
      TURNSTILE_SECRET: "0x4BBB",
    });
    const body = JSON.parse(seen[1]!.init.body);
    assert.deepEqual(body, { name: WIDGET_NAME, domains: ["demo.example.test"], mode: "managed" });
    for (const s of seen) assert.equal(s.init.headers.Authorization, "Bearer tok");
  });

  it("adds a new host to the existing widget and keeps the old ones", async () => {
    const { fetchFn, seen } = scripted({
      [`GET ${BASE}?per_page=100`]: [
        { name: WIDGET_NAME, sitekey: "0x4AAA", domains: ["old.example.test"], mode: "managed" },
      ],
      [`PUT ${BASE}/0x4AAA`]: {},
      [`GET ${BASE}/0x4AAA`]: KEYS,
    });
    const out = await workerSecrets({ ...ENV, NEED_SESSION_KEY: "true" }, fetchFn);
    assert.equal(out.TURNSTILE_SECRET, "0x4BBB");
    assert.match(out.SESSION_KEY ?? "", /^[0-9a-f]{64}$/);
    assert.deepEqual(JSON.parse(seen[1]!.init.body).domains, [
      "old.example.test",
      "demo.example.test",
    ]);
  });

  it("leaves a widget that already allows the host untouched", async () => {
    const { fetchFn, seen } = scripted({
      [`GET ${BASE}?per_page=100`]: [
        { name: WIDGET_NAME, sitekey: "0x4AAA", domains: ["demo.example.test"] },
      ],
      [`GET ${BASE}/0x4AAA`]: KEYS,
    });
    await workerSecrets(ENV, fetchFn);
    assert.deepEqual(
      seen.map((s) => s.key.split(" ")[0]),
      ["GET", "GET"],
    );
  });

  it("fails without leaking the secret when the API refuses or omits keys", async () => {
    await assert.rejects(workerSecrets(ENV, scripted({}).fetchFn), /GET .* failed: 404 10000/);
    await assert.rejects(
      workerSecrets(
        ENV,
        scripted({
          [`GET ${BASE}?per_page=100`]: [],
          [`POST ${BASE}`]: { sitekey: "0x4AAA" },
        }).fetchFn,
      ),
      /no site key or secret/,
    );
    await assert.rejects(
      workerSecrets({ ...ENV, DEMO_HOST: "" }, scripted({}).fetchFn),
      /DEMO_HOST is not set/,
    );
  });

  it("refuses to add a host to a widget that allows the most already", async () => {
    const full = Array.from({ length: 10 }, (_, i) => `h${i}.example.test`);
    const { fetchFn, seen } = scripted({
      [`GET ${BASE}?per_page=100`]: [{ name: WIDGET_NAME, sitekey: "0x4AAA", domains: full }],
    });
    await assert.rejects(workerSecrets(ENV, fetchFn), /most hostnames/);
    assert.equal(seen.length, 1);
  });

  it("makes distinct session keys", () => {
    assert.notEqual(newSessionKey(), newSessionKey());
  });
});
