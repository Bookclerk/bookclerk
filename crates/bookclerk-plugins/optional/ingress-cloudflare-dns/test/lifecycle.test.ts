/**
 * The ingress lifecycle and the entrypoint classes, with the SDK's isolate
 * module replaced by stand-ins (the real one imports `cloudflare:workers`,
 * which only exists inside workerd), and a fake Cloudflare API and address
 * echo behind `fetch`.
 */

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { beforeEach, mock, test } from "node:test";

/** Stand-in for the SDK's `PluginError`. */
class PluginError extends Error {
  /** Wire code. */
  readonly wireCode: string;

  /**
   * Creates an error.
   *
   * @param code - Wire code.
   * @param message - Message.
   */
  constructor(code: string, message: string) {
    super(message);
    this.wireCode = code;
  }

  /**
   * Builds an error from its wire form, as the SDK does.
   *
   * @param code - Wire code.
   * @param message - Message.
   * @returns The error.
   */
  static fromWire(code: string, message: string): PluginError {
    return new PluginError(code, message);
  }
}

/** Stand-in for the SDK's entrypoint base classes. */
class Entrypoint {
  /** Bindings. */
  readonly env: unknown;

  /**
   * Keeps the bindings, as `WorkerEntrypoint` does.
   *
   * @param _ctx - Execution context (unused).
   * @param env - Bindings.
   */
  constructor(_ctx: unknown, env: unknown) {
    this.env = env;
  }
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  exports: { BookclerkEntrypoint: Entrypoint, IngressEntrypoint: Entrypoint, PluginError },
});
const { Ingress, default: Plugin } = await import("../src/index.ts");
const { ECHO_URLS, RECHECK_MS, dnsHealth, learn, liveContext, settingsOf, startDns, stopDns } =
  await import("../src/lifecycle.ts");
const { API_BASE, RECORD_COMMENT, RECORD_TTL } = await import("../src/dns.ts");

/** The zone the fake account holds. */
const ZONE = "example.com";

/** Its id. */
const ZONE_ID = "0123456789abcdef0123456789abcdef";

/** The API token the fake account accepts. */
const TOKEN = "cf-token";

/** One record in the fake zone. */
interface FakeRecord {
  /** Id. */
  id: string;
  /** Type. */
  type: string;
  /** Name. */
  name: string;
  /** Value. */
  content: string;
  /** Proxied. */
  proxied: boolean;
  /** TTL. */
  ttl: number;
  /** Comment. */
  comment: string | null;
}

/** What an echo answers: an address body, a status, or a network failure. */
type Echo = string | { status: number } | "unreachable";

/** The fake world behind `fetch`. */
interface World {
  /** The echo's answer per family. */
  echo: Record<4 | 6, Echo>;
  /** The zone's records. */
  records: FakeRecord[];
  /** API calls, as `METHOD path`. */
  calls: string[];
  /** Echo requests made. */
  echoes: number;
  /** Fails the next API call with this status and message. */
  failNext: { status: number; message: string } | null;
  /** Breaks every API call: no route, a body that errors, or one too long. */
  apiFault: "network" | "stream" | "long" | null;
  /** The clock. */
  clock: number;
  /** The fake `fetch`. */
  fetch: (input: string, init?: RequestInit) => Promise<Response>;
}

/**
 * A JSON API answer.
 *
 * @param status - HTTP status.
 * @param body - The envelope.
 * @returns The response.
 */
function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/**
 * Builds the fake world.
 *
 * @param echo - The echo's answers.
 * @returns The world.
 */
function world(echo: Partial<Record<4 | 6, Echo>> = {}): World {
  let minted = 0;
  const w: World = {
    echo: { 4: "104.16.0.1\n", 6: "2606:4700::1\n", ...echo },
    records: [],
    calls: [],
    echoes: 0,
    failNext: null,
    apiFault: null,
    clock: 1_000_000,
    fetch: async (input, init) => {
      for (const family of [4, 6] as const) {
        if (input !== ECHO_URLS[family]) continue;
        w.echoes += 1;
        const answer = w.echo[family];
        if (answer === "unreachable") throw new TypeError("network");
        if (typeof answer === "string") return new Response(answer);
        return new Response("no", { status: answer.status });
      }
      assert.ok(input.startsWith(API_BASE), input);
      if (w.apiFault === "network") throw new TypeError("network");
      if (w.apiFault === "stream") {
        return new Response(
          new ReadableStream({
            pull(controller) {
              controller.error(new RangeError("stream broke"));
            },
          }),
        );
      }
      if (w.apiFault === "long") return new Response("x".repeat(1024 * 1024 + 1));
      const url = new URL(input);
      const path = url.pathname.slice(new URL(API_BASE).pathname.length);
      const method = init?.method ?? "GET";
      w.calls.push(`${method} ${path}`);
      assert.equal(init?.redirect, "manual");
      const auth = new Headers(init?.headers).get("authorization");
      if (auth !== `Bearer ${TOKEN}`) {
        return json(403, { success: false, errors: [{ code: 9109, message: "Unauthorized" }] });
      }
      if (w.failNext) {
        const { status, message } = w.failNext;
        w.failNext = null;
        return json(status, { success: false, errors: [{ message }] });
      }
      if (path === "/zones") {
        const name = url.searchParams.get("name");
        const result = name === ZONE ? [{ id: ZONE_ID, name: ZONE, status: "active" }] : [];
        return json(200, { success: true, result });
      }
      const base = `/zones/${ZONE_ID}/dns_records`;
      if (path === base && method === "GET") {
        const name = url.searchParams.get("name");
        return json(200, { success: true, result: w.records.filter((r) => r.name === name) });
      }
      const body = init?.body ? (JSON.parse(String(init.body)) as Partial<FakeRecord>) : {};
      if (path === base && method === "POST") {
        const record: FakeRecord = {
          id: (++minted).toString(16).padStart(32, "a"),
          type: String(body.type),
          name: String(body.name),
          content: String(body.content),
          proxied: body.proxied === true,
          ttl: Number(body.ttl),
          comment: body.comment ?? null,
        };
        w.records.push(record);
        return json(200, { success: true, result: record });
      }
      const record = w.records.find((r) => path === `${base}/${r.id}`);
      if (!record) return json(404, { success: false, errors: [{ message: "Not found" }] });
      if (method === "PATCH") {
        Object.assign(record, body);
        return json(200, { success: true, result: record });
      }
      if (method === "DELETE") {
        w.records.splice(w.records.indexOf(record), 1);
        return json(200, { success: true, result: { id: record.id } });
      }
      return json(405, { success: false, errors: [] });
    },
  };
  return w;
}

/**
 * The plugin's bindings.
 *
 * @param config - Settings overrides.
 * @param token - The API token secret, if any.
 * @returns The bindings.
 */
function envOf(
  config: Record<string, unknown> = {},
  token: string | null = TOKEN,
): Parameters<typeof settingsOf>[0] {
  return {
    CONFIG: { hostname: "books.example.com", ipv4: true, ipv6: true, ...config },
    SECRETS: token === null ? {} : { api_token: token },
  } as unknown as Parameters<typeof settingsOf>[0];
}

/**
 * A context over a world.
 *
 * @param w - The world.
 * @param config - Settings overrides.
 * @param token - The API token secret.
 * @returns The context.
 */
function ctxOf(
  w: World,
  config: Record<string, unknown> = {},
  token: string | null = TOKEN,
): Parameters<typeof startDns>[0] {
  return { env: envOf(config, token), fetch: w.fetch, now: () => w.clock };
}

/**
 * The records as `TYPE content`.
 *
 * @param w - The world.
 * @returns Them, sorted.
 */
function summary(w: World): string[] {
  return w.records.map((r) => `${r.type} ${r.content}`).sort();
}

beforeEach(() => {
  stopDns();
});

test("settings: the host name, the families and the token are checked", () => {
  assert.deepEqual(settingsOf(envOf({ hostname: " Books.Example.COM. " })), {
    hostname: "books.example.com",
    families: [4, 6],
    token: TOKEN,
  });
  assert.deepEqual(settingsOf(envOf({ ipv6: false })).families, [4]);
  // IPv6 is off unless turned on.
  assert.deepEqual(settingsOf(envOf({ ipv6: undefined })).families, [4]);
  for (const bad of ["", "localhost", "1.2.3.4", "-a.example.com"]) {
    assert.throws(() => settingsOf(envOf({ hostname: bad })), /hostname/);
  }
  assert.throws(() => settingsOf(envOf({ ipv4: false, ipv6: false })), /ipv4, ipv6/);
  assert.throws(() => settingsOf(envOf({}, null)), /api_token/);
  assert.throws(() => settingsOf(envOf({}, "  ")), /api_token/);
});

test("start creates both records and returns the host name's origin", async () => {
  const w = world();
  assert.deepEqual(await startDns(ctxOf(w)), { origin: "https://books.example.com" });
  assert.deepEqual(summary(w), ["A 104.16.0.1", "AAAA 2606:4700::1"]);
  for (const record of w.records) {
    assert.equal(record.name, "books.example.com");
    assert.equal(record.ttl, RECORD_TTL);
    assert.equal(record.proxied, false);
    assert.equal(record.comment, RECORD_COMMENT);
  }
  // The zone was found under the parent name.
  assert.deepEqual(w.calls.slice(0, 2), ["GET /zones", "GET /zones"]);

  // Starting again with the records current writes nothing.
  w.calls.length = 0;
  await startDns(ctxOf(w));
  assert.ok(
    w.calls.every((call) => call.startsWith("GET")),
    w.calls.join(", "),
  );
});

test("start takes over an existing record and corrects its settings", async () => {
  const w = world();
  w.records.push({
    id: "f".repeat(32),
    type: "A",
    name: "books.example.com",
    content: "93.184.216.34",
    proxied: true,
    ttl: 1,
    comment: null,
  });
  await startDns(ctxOf(w, { ipv6: false }));
  assert.deepEqual(w.records, [
    {
      id: "f".repeat(32),
      type: "A",
      name: "books.example.com",
      content: "104.16.0.1",
      proxied: false,
      ttl: RECORD_TTL,
      comment: RECORD_COMMENT,
    },
  ]);
});

test("start refuses what it cannot keep", async () => {
  const cname = world();
  cname.records.push({
    id: "c".repeat(32),
    type: "CNAME",
    name: "books.example.com",
    content: "elsewhere.example.net",
    proxied: false,
    ttl: 1,
    comment: null,
  });
  await assert.rejects(startDns(ctxOf(cname)), (err: PluginError) => {
    assert.equal(err.wireCode, "unavailable");
    assert.match(err.message, /is a CNAME/);
    return true;
  });

  const twice = world();
  for (const id of ["1", "2"]) {
    twice.records.push({
      id: id.repeat(32),
      type: "A",
      name: "books.example.com",
      content: "93.184.216.34",
      proxied: false,
      ttl: 60,
      comment: null,
    });
  }
  await assert.rejects(startDns(ctxOf(twice)), /has 2 A records/);

  await assert.rejects(startDns(ctxOf(world(), {}, "wrong")), /Zone: DNS: Edit/);
  await assert.rejects(
    startDns(ctxOf(world(), { hostname: "books.example.org" })),
    /no active Cloudflare zone holds books\.example\.org/,
  );
  await assert.rejects(
    startDns(ctxOf(world({ 4: "unreachable", 6: { status: 503 } }))),
    /no IPv4 route.*IPv6 address echo gave no address/,
  );
  await assert.rejects(startDns(ctxOf(world(), { hostname: "" })), (err: PluginError) => {
    assert.equal(err.wireCode, "invalid_params");
    return true;
  });

  const failing = world();
  failing.failNext = { status: 500, message: "internal" };
  await assert.rejects(startDns(ctxOf(failing)), /Cloudflare answered 500: internal/);
});

test("health follows an address change, rechecks the records now and then, and reports a missing family", async () => {
  const w = world();
  await startDns(ctxOf(w));
  w.calls.length = 0;

  // Nothing changed: two echo requests and no API call.
  const echoes = w.echoes;
  assert.deepEqual(await dnsHealth(ctxOf(w)), { serving: true });
  assert.equal(w.echoes, echoes + 2);
  assert.deepEqual(w.calls, []);

  // The IPv4 address moved: the A record follows.
  w.echo[4] = "151.101.1.1\n";
  assert.deepEqual(await dnsHealth(ctxOf(w)), { serving: true });
  assert.deepEqual(summary(w), ["A 151.101.1.1", "AAAA 2606:4700::1"]);

  // Someone edits the record by hand: the next recheck corrects it.
  (w.records.find((r) => r.type === "A") as FakeRecord).content = "192.0.2.99";
  w.calls.length = 0;
  await dnsHealth(ctxOf(w));
  assert.deepEqual(w.calls, []);
  w.clock += RECHECK_MS;
  await dnsHealth(ctxOf(w));
  assert.deepEqual(summary(w), ["A 151.101.1.1", "AAAA 2606:4700::1"]);

  // IPv6 goes away: still serving on IPv4, and the AAAA record is kept.
  w.echo[6] = "unreachable";
  assert.deepEqual(await dnsHealth(ctxOf(w)), {
    serving: true,
    detail: "this computer has no IPv6 route to the internet",
  });
  assert.deepEqual(summary(w), ["A 151.101.1.1", "AAAA 2606:4700::1"]);

  // Both gone: not serving.
  w.echo[4] = "10.0.0.5\n";
  const health = await dnsHealth(ctxOf(w));
  assert.equal(health.serving, false);
  assert.match(health.detail ?? "", /IPv4 address 10\.0\.0\.5 is not public/);
});

test("turning a family off takes down only the record the plugin kept", async () => {
  const w = world();
  await startDns(ctxOf(w));
  w.records.push({
    id: "e".repeat(32),
    type: "AAAA",
    name: "books.example.com",
    content: "2606:4700::99",
    proxied: false,
    ttl: 60,
    comment: "mine",
  });
  w.clock += RECHECK_MS;
  assert.deepEqual(await dnsHealth(ctxOf(w, { ipv6: false })), { serving: true });
  assert.deepEqual(summary(w), ["A 104.16.0.1", "AAAA 2606:4700::99"]);
});

test("health reports bad settings and API failures without throwing", async () => {
  assert.deepEqual(await dnsHealth(ctxOf(world(), {}, null)), {
    serving: false,
    detail: "set the api_token secret to a Cloudflare API token",
  });
  const w = world();
  w.failNext = { status: 429, message: "slow down" };
  assert.deepEqual(await dnsHealth(ctxOf(w)), {
    serving: false,
    detail: "Cloudflare answered 429: slow down",
  });
});

test("an API that cannot be reached or reads badly", async () => {
  const w = world();
  w.apiFault = "network";
  await assert.rejects(startDns(ctxOf(w)), /could not reach the Cloudflare API/);
  w.apiFault = "long";
  await assert.rejects(startDns(ctxOf(w)), /Cloudflare answered 200 at length/);
  // A failure that is not the API's own passes through unchanged.
  w.apiFault = "stream";
  await assert.rejects(startDns(ctxOf(w)), RangeError);
  await assert.rejects(dnsHealth(ctxOf(w)), RangeError);
});

test("the echo: answers that are not one public address of the family", async () => {
  const w = world();
  w.echo[4] = "104.16.0.1 \n";
  assert.deepEqual(await learn(w.fetch, 4), {
    problem: "the IPv4 address echo gave no address",
  });
  w.echo[4] = "x".repeat(100);
  assert.deepEqual(await learn(w.fetch, 4), {
    problem: "the IPv4 address echo gave no address",
  });
  w.echo[6] = "2001:DB8::1\n";
  assert.deepEqual(await learn(w.fetch, 6), {
    problem: "this computer's IPv6 address 2001:db8::1 is not public",
  });
  w.echo[6] = "2606:4700:0:0:0:0:0:0001\n";
  assert.deepEqual(await learn(w.fetch, 6), { address: "2606:4700::1" });
});

test("the entrypoint classes call the lifecycle", async () => {
  const w = world();
  const original = globalThis.fetch;
  globalThis.fetch = w.fetch as typeof fetch;
  try {
    const ingress = new Ingress({} as never, envOf() as never);
    assert.deepEqual(await ingress.start(), { origin: "https://books.example.com" });
    assert.deepEqual(await ingress.health(), { serving: true });
    await ingress.stop();
    assert.ok(new Plugin({} as never, envOf() as never));
    const ctx = liveContext(envOf());
    assert.ok(ctx.now() > 0);
    await ctx.fetch(ECHO_URLS[4]);
  } finally {
    globalThis.fetch = original;
  }
});

test("the manifest's module list matches src/", () => {
  const manifest = readFileSync(new URL("../plugin.toml", import.meta.url), "utf8");
  const listed = [...manifest.matchAll(/^name = "([a-z-]+)\.js"$/gm)].map((m) => m[1]).sort();
  assert.deepEqual(listed, ["address", "dns", "index", "lifecycle"]);
});
