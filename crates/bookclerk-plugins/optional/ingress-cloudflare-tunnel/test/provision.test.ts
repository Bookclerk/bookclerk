/**
 * The guided setup and its removal, with the SDK's isolate module replaced
 * by stand-ins and a fake Cloudflare API (accounts, tunnels, zones and DNS
 * records) behind `fetch`.
 */

import assert from "node:assert/strict";
import { mock, test } from "node:test";

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

/**
 * The SDK's `provisioned`, restated.
 *
 * @param answer - Settings, secrets and summary.
 * @returns The wire shape.
 */
function provisioned(answer: {
  settings?: Record<string, unknown>;
  secrets?: Record<string, string>;
  summary?: string[];
}): { settingsJson: string; secrets: { key: string; value: string }[]; summary: string[] } {
  return {
    settingsJson: JSON.stringify(answer.settings ?? {}),
    secrets: Object.entries(answer.secrets ?? {}).map(([key, value]) => ({ key, value })),
    summary: answer.summary ?? [],
  };
}

mock.module("@bookclerk/plugin-sdk/workerd", {
  exports: {
    BookclerkEntrypoint: Entrypoint,
    IngressEntrypoint: Entrypoint,
    ProvisioningEntrypoint: Entrypoint,
    PluginError,
    provisioned,
  },
});
const { Provisioning } = await import("../src/index.ts");
const {
  RECORD_COMMENT,
  deprovisionTunnel,
  grantToken,
  provisionTunnel,
  tunnelConfig,
  tunnelName,
  tunnelTarget,
  uuidOf,
} = await import("../src/provision.ts");
const { API_BASE, MAX_ANSWER_BYTES, zoneCandidates } = await import("../src/cfapi.ts");

/** The fake account's id. */
const ACCOUNT = "3f4b7e3dcab231cbfdaa90a6a28bd548";

/** The zone's id. */
const ZONE_ID = "0123456789abcdef0123456789abcdef";

/** The token the operator pastes. */
const PASTED = "pasted-token";

/** A tunnel in the fake account. */
interface FakeTunnel {
  /** Id. */
  id: string;
  /** Name. */
  name: string;
  /** When it was deleted, or `null`. */
  deleted_at: string | null;
  /** Its routing, once set. */
  config?: unknown;
}

/** A record in the fake zone. */
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
  /** Comment. */
  comment: string | null;
}

/** The fake world behind `fetch`. */
interface World {
  /** The account's tunnels. */
  tunnels: FakeTunnel[];
  /** The zone's records. */
  records: FakeRecord[];
  /** Calls, as `METHOD path`. */
  calls: string[];
  /** The accounts `GET /accounts` lists. */
  accounts: string[];
  /** Fails calls whose `METHOD path` starts with this, with the status. */
  fail: { prefix: string; status: number } | null;
  /** Replaces the token answer's result. */
  tokenAnswer: unknown;
  /** Replaces the tunnel create answer's result. */
  createAnswer: unknown;
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
  return new Response(JSON.stringify(body), { status });
}

/**
 * The fake's refusal.
 *
 * @param status - HTTP status.
 * @param message - Error message.
 * @returns The response.
 */
function refused(status: number, message: string): Response {
  return json(status, { success: false, errors: [{ code: 1, message }] });
}

/**
 * The token Cloudflare hands out for a tunnel.
 *
 * @param account - The account's id.
 * @param tunnel - The tunnel's id.
 * @returns The base64 token.
 */
function tunnelToken(account: string, tunnel: string): string {
  const secret = Buffer.alloc(32, 7).toString("base64");
  return Buffer.from(JSON.stringify({ a: account, t: tunnel, s: secret })).toString("base64");
}

/**
 * Builds the fake world.
 *
 * @returns The world.
 */
function world(): World {
  let next = 0;
  const w: World = {
    tunnels: [],
    records: [],
    calls: [],
    accounts: [ACCOUNT],
    fail: null,
    tokenAnswer: undefined,
    createAnswer: undefined,
    fetch: async (input, init) => {
      assert.ok(input.startsWith(API_BASE), input);
      assert.equal(init?.redirect, "manual");
      const url = new URL(input);
      const path = url.pathname.slice(new URL(API_BASE).pathname.length);
      const method = init?.method ?? "GET";
      w.calls.push(`${method} ${path}`);
      if (new Headers(init?.headers).get("authorization") !== `Bearer ${PASTED}`) {
        return refused(401, "Invalid API Token");
      }
      if (w.fail && `${method} ${path}`.startsWith(w.fail.prefix)) {
        return refused(w.fail.status, "Refused");
      }
      const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
      if (path === "/accounts") {
        return json(200, { success: true, result: w.accounts.map((id) => ({ id })) });
      }
      if (path === "/zones") {
        const name = url.searchParams.get("name");
        const result = name === "example.com" ? [{ id: ZONE_ID, name, status: "active" }] : [];
        return json(200, { success: true, result });
      }
      const records = `/zones/${ZONE_ID}/dns_records`;
      if (path === records && method === "GET") {
        const name = url.searchParams.get("name");
        return json(200, { success: true, result: w.records.filter((r) => r.name === name) });
      }
      if (path === records && method === "POST") {
        const record = {
          id: (++next).toString(16).padStart(32, "b"),
          type: String(body.type),
          name: String(body.name),
          content: String(body.content),
          proxied: body.proxied === true,
          comment: typeof body.comment === "string" ? body.comment : null,
        };
        w.records.push(record);
        return json(200, { success: true, result: record });
      }
      const record = w.records.find((r) => path === `${records}/${r.id}`);
      if (record && method === "PATCH") {
        Object.assign(record, body);
        return json(200, { success: true, result: record });
      }
      if (record && method === "DELETE") {
        w.records.splice(w.records.indexOf(record), 1);
        return json(200, { success: true, result: { id: record.id } });
      }
      const tunnels = `/accounts/${ACCOUNT}/cfd_tunnel`;
      if (path === tunnels && method === "GET") {
        const name = url.searchParams.get("name");
        const live = url.searchParams.get("is_deleted") === "false";
        const result = w.tunnels.filter((t) => t.name === name && (!live || t.deleted_at === null));
        return json(200, { success: true, result });
      }
      if (path === tunnels && method === "POST") {
        assert.equal(body.config_src, "cloudflare");
        const tunnel = {
          id: `${(++next).toString(16).padStart(8, "0")}-0000-4000-8000-000000000000`,
          name: String(body.name),
          deleted_at: null,
        };
        w.tunnels.push(tunnel);
        return json(200, {
          success: true,
          result: w.createAnswer === undefined ? tunnel : w.createAnswer,
        });
      }
      const tunnel = w.tunnels.find((t) => path.startsWith(`${tunnels}/${t.id}`));
      if (!tunnel) return refused(404, "Tunnel not found");
      const at = `${tunnels}/${tunnel.id}`;
      if (path === at && method === "GET") return json(200, { success: true, result: tunnel });
      if (path === at && method === "DELETE") {
        tunnel.deleted_at = "2026-11-24T00:00:00Z";
        return json(200, { success: true, result: tunnel });
      }
      if (path === `${at}/configurations` && method === "PUT") {
        tunnel.config = body;
        return json(200, { success: true, result: {} });
      }
      if (path === `${at}/token`) {
        const result =
          w.tokenAnswer === undefined ? tunnelToken(ACCOUNT, tunnel.id) : w.tokenAnswer;
        return json(200, { success: true, result });
      }
      if (path === `${at}/connections` && method === "DELETE") {
        return json(200, { success: true, result: null });
      }
      return refused(404, "No route");
    },
  };
  return w;
}

/**
 * Bindings with settings and secrets.
 *
 * @param config - Settings over the defaults.
 * @param secrets - Stored secrets.
 * @returns The bindings.
 */
function envOf(
  config: Record<string, unknown> = {},
  secrets: Record<string, string> = {},
): Parameters<typeof provisionTunnel>[0]["env"] {
  return {
    CONFIG: { hostname: "books.example.com", account_id: "", ...config },
    SECRETS: secrets,
  } as unknown as Parameters<typeof provisionTunnel>[0]["env"];
}

/** A grant carrying the pasted token. */
const GRANT = { grant: { kind: "apiToken" as const, token: PASTED }, inputsJson: "{}" };

/** No grant. */
const NONE = { grant: { kind: "none" as const } };

/**
 * Runs `fn` and returns the plugin error it throws.
 *
 * @param fn - The call.
 * @returns The error.
 */
async function refusal(fn: () => Promise<unknown>): Promise<PluginError> {
  try {
    await fn();
  } catch (err) {
    assert.ok(err instanceof PluginError, String(err));
    return err;
  }
  assert.fail("expected a refusal");
}

test("the setup creates the tunnel, routes the name and points DNS at it", async () => {
  const w = world();
  const reply = await provisionTunnel(
    { env: envOf({ hostname: "Books.Example.com." }), fetch: w.fetch },
    GRANT,
  );
  const [tunnel] = w.tunnels;
  assert.ok(tunnel);
  assert.equal(tunnel.name, tunnelName("books.example.com"));
  assert.deepEqual(tunnel.config, tunnelConfig("books.example.com"));
  assert.deepEqual(JSON.parse(reply.settingsJson), {
    account_id: ACCOUNT,
    hostname: "books.example.com",
  });
  assert.deepEqual(reply.secrets, [{ key: "token", value: tunnelToken(ACCOUNT, tunnel.id) }]);
  assert.deepEqual(reply.summary, [
    "Created the tunnel bookclerk-books.example.com",
    "Routed books.example.com to it",
    "Pointed books.example.com at the tunnel",
    "Kept the tunnel's own token, which can run this tunnel only",
  ]);
  assert.deepEqual(
    w.records.map(({ type, name, content, proxied, comment }) => ({
      type,
      name,
      content,
      proxied,
      comment,
    })),
    [
      {
        type: "CNAME",
        name: "books.example.com",
        content: tunnelTarget(tunnel.id),
        proxied: true,
        comment: RECORD_COMMENT,
      },
    ],
  );
  assert.ok(!JSON.stringify(reply).includes(PASTED));

  // Again, with the stored token: the same tunnel, nothing new.
  const env = envOf({ account_id: "" }, { token: tunnelToken(ACCOUNT, tunnel.id) });
  const again = await provisionTunnel({ env, fetch: w.fetch }, GRANT);
  assert.equal(w.tunnels.length, 1);
  assert.equal(again.summary[0], "Found the tunnel bookclerk-books.example.com");
  assert.equal(again.summary[2], "books.example.com already points at the tunnel");
  // The account came from the stored token, not the account list.
  assert.ok(!w.calls.slice(-8).includes("GET /accounts"));
});

test("a tunnel is found by name when the stored token is gone or stale", async () => {
  const w = world();
  w.tunnels.push(
    {
      id: "11111111-0000-4000-8000-000000000000",
      name: tunnelName("books.example.com"),
      deleted_at: "x",
    },
    {
      id: "22222222-0000-4000-8000-000000000000",
      name: tunnelName("books.example.com"),
      deleted_at: null,
    },
  );
  // A stored token for a deleted tunnel, and one for another account.
  for (const secrets of [
    { token: tunnelToken(ACCOUNT, "11111111-0000-4000-8000-000000000000") },
    { token: tunnelToken("f".repeat(32), "33333333-0000-4000-8000-000000000000") },
    { token: "not a token" },
  ]) {
    const reply = await provisionTunnel(
      { env: envOf({ account_id: ACCOUNT }, secrets), fetch: w.fetch },
      GRANT,
    );
    assert.equal(reply.summary[0], "Found the tunnel bookclerk-books.example.com");
    assert.equal(
      reply.secrets[0]?.value,
      tunnelToken(ACCOUNT, "22222222-0000-4000-8000-000000000000"),
    );
  }
  assert.equal(w.tunnels.length, 2);
});

test("an existing CNAME is taken over only when it is a tunnel's", async () => {
  const w = world();
  const cname = {
    id: "c".repeat(32),
    type: "CNAME",
    name: "books.example.com",
    content: "old.cfargotunnel.com",
    proxied: false,
    comment: null,
  };
  w.records.push({ ...cname });
  const reply = await provisionTunnel({ env: envOf(), fetch: w.fetch }, GRANT);
  assert.equal(reply.summary[2], "Pointed books.example.com at the tunnel");
  assert.equal(w.records[0]?.content, tunnelTarget(w.tunnels[0]?.id ?? ""));
  assert.equal(w.records[0]?.comment, RECORD_COMMENT);

  w.records = [{ ...cname, content: "elsewhere.example.net" }];
  const taken = await refusal(() => provisionTunnel({ env: envOf(), fetch: w.fetch }, GRANT));
  assert.equal(taken.wireCode, "invalid_params");
  assert.match(taken.message, /already points at elsewhere\.example\.net/);

  w.records = [{ ...cname, type: "A", content: "104.16.0.1" }];
  assert.match(
    (await refusal(() => provisionTunnel({ env: envOf(), fetch: w.fetch }, GRANT))).message,
    /already has A records/,
  );
});

test("the setup refuses bad choices and reports Cloudflare's refusals", async () => {
  const w = world();
  const ctx = (config: Record<string, unknown> = {}): Parameters<typeof provisionTunnel>[0] => ({
    env: envOf(config),
    fetch: w.fetch,
  });
  assert.equal(
    (await refusal(() => provisionTunnel(ctx(), { grant: NONE.grant, inputsJson: "{}" }))).wireCode,
    "invalid_params",
  );
  assert.equal(
    (await refusal(() => provisionTunnel(ctx({ hostname: "nodot" }), GRANT))).wireCode,
    "invalid_params",
  );
  const noZone = await refusal(() =>
    provisionTunnel(ctx({ hostname: "books.example.org" }), GRANT),
  );
  assert.equal(noZone.wireCode, "invalid_params");
  assert.match(noZone.message, /no active Cloudflare zone/);

  for (const [prefix, status, code, text] of [
    [
      "POST /accounts/",
      403,
      "forbidden",
      /could not create the tunnel: Cloudflare answered 403: Refused/,
    ],
    ["PUT ", 500, "unavailable", /could not route the host name/],
    ["POST /zones/", 400, "unavailable", /could not create the DNS record/],
  ] as const) {
    w.fail = { prefix, status };
    const err = await refusal(() => provisionTunnel(ctx(), GRANT));
    assert.equal(err.wireCode, code);
    assert.match(err.message, text);
    w.fail = null;
    w.tunnels.length = 0;
    w.records.length = 0;
  }
  w.records.push({
    id: "c".repeat(32),
    type: "CNAME",
    name: "books.example.com",
    content: "old.cfargotunnel.com",
    proxied: true,
    comment: null,
  });
  w.fail = { prefix: "PATCH ", status: 400 };
  assert.match(
    (await refusal(() => provisionTunnel(ctx(), GRANT))).message,
    /could not update the DNS record/,
  );
  w.fail = null;

  w.createAnswer = { id: "nope", name: "x" };
  w.tunnels.length = 0;
  assert.match(
    (await refusal(() => provisionTunnel(ctx(), GRANT))).message,
    /new tunnel has no usable id/,
  );
  w.createAnswer = undefined;
  w.tokenAnswer = "has space";
  assert.match(
    (await refusal(() => provisionTunnel(ctx(), GRANT))).message,
    /tunnel token is not usable/,
  );
  w.tokenAnswer = undefined;

  w.accounts.push("f".repeat(32));
  assert.match(
    (await refusal(() => provisionTunnel(ctx(), GRANT))).message,
    /more than one account/,
  );
  w.accounts.length = 0;
  assert.equal((await refusal(() => provisionTunnel(ctx(), GRANT))).wireCode, "forbidden");
  w.accounts.push(ACCOUNT);
  assert.equal(
    (await refusal(() => provisionTunnel(ctx({ account_id: "nope" }), GRANT))).wireCode,
    "invalid_params",
  );

  const raw = (body: string) => async (): Promise<Response> => new Response(body);
  for (const [body, text] of [
    ["not json", /without JSON/],
    ["[]", /without an API envelope/],
    ["x".repeat(MAX_ANSWER_BYTES + 1), /at length/],
    ['{"success":true,"result":{}}', /account list is not a list/],
  ] as const) {
    const err = await refusal(() => provisionTunnel({ env: envOf(), fetch: raw(body) }, GRANT));
    assert.match(err.message, text);
  }
  const offline = await refusal(() =>
    provisionTunnel(
      {
        env: envOf(),
        fetch: async () => {
          throw new TypeError("offline");
        },
      },
      GRANT,
    ),
  );
  assert.match(offline.message, /could not reach the Cloudflare API/);
  await assert.rejects(
    provisionTunnel(
      {
        env: envOf(),
        fetch: async () =>
          new Response(
            new ReadableStream({
              pull(controller) {
                controller.error(new RangeError("stream broke"));
              },
            }),
          ),
      },
      GRANT,
    ),
    RangeError,
  );
});

test("removal deletes the DNS record and the tunnel, with a pasted token", async () => {
  const w = world();
  const reply = await provisionTunnel({ env: envOf(), fetch: w.fetch }, GRANT);
  const secrets = { token: reply.secrets[0]?.value ?? "" };
  w.records.push({
    id: "d".repeat(32),
    type: "CNAME",
    name: "books.example.com",
    content: "someone-else.example.net",
    proxied: true,
    comment: null,
  });
  w.calls.length = 0;
  const err = await refusal(() =>
    deprovisionTunnel({ env: envOf({}, secrets), fetch: w.fetch }, NONE),
  );
  assert.equal(err.wireCode, "unauthorized");
  assert.equal(w.calls.length, 0);
  await deprovisionTunnel({ env: envOf({}, secrets), fetch: w.fetch }, GRANT);
  assert.deepEqual(
    w.records.map((r) => r.content),
    ["someone-else.example.net"],
  );
  assert.notEqual(w.tunnels[0]?.deleted_at, null);
  assert.ok(w.calls.some((call) => call.endsWith("/connections")));

  // Nothing stored: nothing to do.
  w.calls.length = 0;
  await deprovisionTunnel({ env: envOf(), fetch: w.fetch }, NONE);
  assert.equal(w.calls.length, 0);
  // A bad host name skips the DNS record; a tunnel already gone is fine.
  await deprovisionTunnel(
    {
      env: envOf(
        { hostname: "" },
        { token: tunnelToken(ACCOUNT, "44444444-0000-4000-8000-000000000000") },
      ),
      fetch: w.fetch,
    },
    GRANT,
  );
  assert.ok(!w.calls.some((call) => call.startsWith("GET /zones")));

  // Refusals are reported.
  for (const prefix of ["DELETE /zones/", "DELETE /accounts/"]) {
    w.records.push({
      id: "e".repeat(32),
      type: "CNAME",
      name: "books.example.com",
      content: tunnelTarget(w.tunnels[0]?.id ?? ""),
      proxied: true,
      comment: RECORD_COMMENT,
    });
    w.fail = { prefix, status: 403 };
    const refusedDelete = await refusal(() =>
      deprovisionTunnel({ env: envOf({}, secrets), fetch: w.fetch }, GRANT),
    );
    assert.equal(refusedDelete.wireCode, "forbidden");
    w.fail = null;
    w.records.length = 0;
  }
  // The tunnel delete itself refused, after its connections went.
  const first = w.tunnels[0];
  assert.ok(first);
  first.deleted_at = null;
  const gate = w.fetch;
  w.fetch = async (input, init) =>
    init?.method === "DELETE" && input.endsWith(w.tunnels[0]?.id ?? "-")
      ? refused(409, "Tunnel has active connections")
      : gate(input, init);
  const busy = await refusal(() =>
    deprovisionTunnel({ env: envOf({}, secrets), fetch: w.fetch }, GRANT),
  );
  assert.match(busy.message, /could not delete the tunnel: Cloudflare answered 409/);
});

test("helpers: ids, grants and zone candidates", () => {
  assert.equal(
    uuidOf(Uint8Array.from({ length: 16 }, (_, i) => i)),
    "00010203-0405-0607-0809-0a0b0c0d0e0f",
  );
  assert.equal(grantToken({ kind: "none", token: "x" }), null);
  assert.equal(grantToken({ kind: "apiToken" }), null);
  assert.equal(grantToken({ kind: "apiToken", token: " t " }), "t");
  assert.deepEqual(zoneCandidates("a.b.example.com"), [
    "a.b.example.com",
    "b.example.com",
    "example.com",
  ]);
});

test("the provisioning class calls the setup", async () => {
  const w = world();
  const original = globalThis.fetch;
  globalThis.fetch = w.fetch as typeof fetch;
  try {
    const entry = new Provisioning({} as never, envOf() as never);
    const reply = await entry.provision(GRANT);
    assert.equal(reply.secrets[0]?.key, "token");
    await entry.deprovision(NONE);
  } finally {
    globalThis.fetch = original;
  }
});
