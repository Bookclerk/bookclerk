/**
 * The guided setup and its removal, with the SDK's isolate module replaced
 * by stand-ins and a fake Cloudflare API (zones, records, accounts and
 * account API tokens) behind `fetch`.
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
const { SETUP_HINT, deprovisionDns, grantToken, provisionDns, tokenName, zoneResource } =
  await import("../src/provision.ts");
const { API_BASE, RECORD_COMMENT } = await import("../src/dns.ts");
const { SetupError, findAccount, tokenRequest } = await import("../src/tokens.ts");

/** The fake account's id. */
const ACCOUNT = "3f4b7e3dcab231cbfdaa90a6a28bd548";

/** The zone's id. */
const ZONE_ID = "0123456789abcdef0123456789abcdef";

/** The token the operator pastes: it may do everything. */
const PASTED = "pasted-token";

/** The token stored before the setup ran: DNS only. */
const STORED = "stored-token";

/** Permission group ids by name. */
const GROUPS: Record<string, string> = {
  "Zone Read": "c".repeat(32),
  "DNS Write": "d".repeat(32),
};

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
  /** Comment. */
  comment: string;
}

/** The fake world behind `fetch`. */
interface World {
  /** The zone's records. */
  records: FakeRecord[];
  /** Live account token ids by value. */
  tokens: Map<string, string>;
  /** Bodies of token create calls. */
  minted: Record<string, unknown>[];
  /** Calls, as `METHOD path (token)`. */
  calls: string[];
  /** The accounts `GET /accounts` lists. */
  accounts: string[];
  /** Whether the zone answer names its account. */
  zoneAccount: boolean;
  /** The groups offered. */
  offered: Set<string>;
  /** Replaces the token create answer's result. */
  mintAnswer: unknown;
  /** Makes token deletes fail with 500. */
  deleteFails: boolean;
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
 * Builds the fake world.
 *
 * @returns The world.
 */
function world(): World {
  let next = 0;
  const w: World = {
    records: [],
    tokens: new Map([[STORED, "e".repeat(32)]]),
    minted: [],
    calls: [],
    accounts: [ACCOUNT],
    zoneAccount: true,
    offered: new Set(Object.keys(GROUPS)),
    mintAnswer: undefined,
    deleteFails: false,
    fetch: async (input, init) => {
      assert.ok(input.startsWith(API_BASE), input);
      assert.equal(init?.redirect, "manual");
      const url = new URL(input);
      const path = url.pathname.slice(new URL(API_BASE).pathname.length);
      const method = init?.method ?? "GET";
      const token = (new Headers(init?.headers).get("authorization") ?? "").replace(/^Bearer /, "");
      w.calls.push(`${method} ${path} (${token === PASTED ? "pasted" : token})`);
      const pasted = token === PASTED;
      if (!pasted && !w.tokens.has(token)) return refused(401, "Invalid API Token");
      if (path === "/zones") {
        const name = url.searchParams.get("name");
        const zone = { id: ZONE_ID, name: "example.com", status: "active" };
        const result =
          name === "example.com"
            ? [w.zoneAccount ? { ...zone, account: { id: ACCOUNT } } : zone]
            : [];
        return json(200, { success: true, result });
      }
      const records = `/zones/${ZONE_ID}/dns_records`;
      if (path === records && method === "GET") {
        const name = url.searchParams.get("name");
        return json(200, { success: true, result: w.records.filter((r) => r.name === name) });
      }
      const record = w.records.find((r) => path === `${records}/${r.id}`);
      if (record && method === "DELETE") {
        w.records.splice(w.records.indexOf(record), 1);
        return json(200, { success: true, result: { id: record.id } });
      }
      // Everything below needs the pasted token.
      if (!pasted) return refused(403, "Unauthorized to access requested resource");
      if (path === "/accounts") {
        return json(200, { success: true, result: w.accounts.map((id) => ({ id, name: "x" })) });
      }
      const tokens = `/accounts/${ACCOUNT}/tokens`;
      if (path === `${tokens}/permission_groups`) {
        const name = url.searchParams.get("name") ?? "";
        const result = w.offered.has(name) ? [{ id: GROUPS[name], name, scopes: [] }] : [];
        return json(200, { success: true, result });
      }
      if (path === tokens && method === "POST") {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        w.minted.push(body);
        const id = (++next).toString(16).padStart(32, "a");
        const value = `minted-${next}`;
        w.tokens.set(value, id);
        const result = w.mintAnswer === undefined ? { id, value, name: body.name } : w.mintAnswer;
        return json(200, { success: true, result });
      }
      if (path.startsWith(`${tokens}/`) && method === "DELETE") {
        if (w.deleteFails) return refused(500, "Internal error");
        const id = path.slice(tokens.length + 1);
        const entry = [...w.tokens].find(([, value]) => value === id);
        if (!entry) return refused(404, "Token not found");
        w.tokens.delete(entry[0]);
        return json(200, { success: true, result: { id } });
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
): Parameters<typeof provisionDns>[0]["env"] {
  return {
    CONFIG: { hostname: "books.example.com", ipv4: true, ipv6: false, ...config },
    SECRETS: secrets,
  } as unknown as Parameters<typeof provisionDns>[0]["env"];
}

/** A grant carrying the pasted token. */
const GRANT = { grant: { kind: "apiToken" as const, token: ` ${PASTED} ` }, inputsJson: "{}" };

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

test("the setup mints a token for the zone alone and keeps it", async () => {
  const w = world();
  const reply = await provisionDns({ env: envOf(), fetch: w.fetch }, GRANT);
  assert.deepEqual(JSON.parse(reply.settingsJson), {
    hostname: "books.example.com",
    api_token_id: "a".repeat(31) + "1",
  });
  assert.deepEqual(reply.secrets, [{ key: "api_token", value: "minted-1" }]);
  assert.deepEqual(reply.summary, [
    "Found the zone that holds books.example.com",
    "Created an API token that can edit DNS in that zone only",
    "The records are written when the ingress starts",
  ]);
  assert.deepEqual(w.minted, [
    tokenRequest(
      tokenName("books.example.com"),
      [GROUPS["Zone Read"] as string, GROUPS["DNS Write"] as string],
      zoneResource(ZONE_ID),
    ),
  ]);
  assert.deepEqual(w.minted[0], {
    name: "Bookclerk cloudflare_dns books.example.com",
    policies: [
      {
        effect: "allow",
        permission_groups: [{ id: "c".repeat(32) }, { id: "d".repeat(32) }],
        resources: { [`com.cloudflare.api.account.zone.${ZONE_ID}`]: "*" },
      },
    ],
  });
  // Every call used the pasted token, and it is not in the reply.
  assert.ok(
    w.calls.every((call) => call.endsWith("(pasted)")),
    w.calls.join(", "),
  );
  assert.ok(!JSON.stringify(reply).includes(PASTED));
});

test("a second setup rotates the token it minted", async () => {
  const w = world();
  const first = await provisionDns({ env: envOf(), fetch: w.fetch }, GRANT);
  const firstId = JSON.parse(first.settingsJson).api_token_id as string;
  const second = await provisionDns(
    { env: envOf({ api_token_id: firstId }, { api_token: "minted-1" }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(second.summary.includes("Deleted the token an earlier setup created"));
  assert.deepEqual([...w.tokens.keys()].sort(), ["minted-2", STORED]);

  // A token that is already gone is not mentioned.
  const third = await provisionDns(
    { env: envOf({ api_token_id: firstId }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(!third.summary.some((line) => line.includes("earlier")), third.summary.join("; "));

  // One that cannot be deleted is named, and the setup still succeeds.
  w.deleteFails = true;
  const fourth = await provisionDns(
    { env: envOf({ api_token_id: "b".repeat(32) }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(
    fourth.summary.includes(
      `Could not delete the earlier token ${"b".repeat(32)}; delete it in the Cloudflare dashboard`,
    ),
  );
  // A malformed stored id is ignored.
  w.deleteFails = false;
  const fifth = await provisionDns(
    { env: envOf({ api_token_id: "not-an-id" }), fetch: w.fetch },
    GRANT,
  );
  assert.equal(fifth.summary.length, 3);
});

test("the setup refuses bad choices, a missing grant and a token that cannot do it", async () => {
  const w = world();
  const ctx = (config: Record<string, unknown> = {}): Parameters<typeof provisionDns>[0] => ({
    env: envOf(config),
    fetch: w.fetch,
  });
  for (const grant of [NONE.grant, { kind: "apiToken" as const, token: "  " }]) {
    const err = await refusal(() => provisionDns(ctx(), { grant, inputsJson: "{}" }));
    assert.equal(err.wireCode, "invalid_params");
  }
  assert.equal(
    (await refusal(() => provisionDns(ctx({ hostname: "x" }), GRANT))).wireCode,
    "invalid_params",
  );
  assert.equal(
    (await refusal(() => provisionDns(ctx({ ipv4: false, ipv6: false }), GRANT))).wireCode,
    "invalid_params",
  );
  // IPv6 alone is fine.
  await provisionDns(ctx({ ipv4: false, ipv6: true }), GRANT);

  const noZone = await refusal(() => provisionDns(ctx({ hostname: "books.example.org" }), GRANT));
  assert.equal(noZone.wireCode, "unavailable");
  assert.match(noZone.message, /no active Cloudflare zone/);

  w.offered.delete("DNS Write");
  const noGroup = await refusal(() => provisionDns(ctx(), GRANT));
  assert.equal(noGroup.wireCode, "forbidden");
  assert.match(noGroup.message, /cannot grant "DNS Write"/);
  w.offered.add("DNS Write");

  w.zoneAccount = false;
  assert.match(
    (await refusal(() => provisionDns(ctx(), GRANT))).message,
    /did not say which account owns/,
  );
  w.zoneAccount = true;

  w.mintAnswer = { id: "a".repeat(32), value: "has space" };
  assert.match((await refusal(() => provisionDns(ctx(), GRANT))).message, /no usable value/);
  w.mintAnswer = { id: "short", value: "v" };
  assert.match((await refusal(() => provisionDns(ctx(), GRANT))).message, /no usable id/);
  w.mintAnswer = "nope";
  assert.match((await refusal(() => provisionDns(ctx(), GRANT))).message, /not an object/);
  w.mintAnswer = undefined;

  // A pasted token Cloudflare does not know: the hint names the permissions.
  const bad = await refusal(() =>
    provisionDns(ctx(), { grant: { kind: "apiToken", token: "wrong" }, inputsJson: "{}" }),
  );
  assert.equal(bad.wireCode, "unavailable");
  assert.ok(bad.message.endsWith(SETUP_HINT), bad.message);
});

test("removal deletes the kept records and the minted token", async () => {
  const w = world();
  const ours = {
    type: "A",
    name: "books.example.com",
    content: "104.16.0.1",
    comment: RECORD_COMMENT,
  };
  w.records.push(
    { id: "1".repeat(32), ...ours },
    { id: "2".repeat(32), ...ours, type: "AAAA", content: "2606:4700::1" },
    { id: "3".repeat(32), ...ours, comment: "someone else's" },
    { id: "4".repeat(32), ...ours, type: "TXT", content: "x" },
  );
  // Without a minted token, the stored token removes the records.
  await deprovisionDns({ env: envOf({}, { api_token: STORED }), fetch: w.fetch }, NONE);
  assert.deepEqual(
    w.records.map((r) => r.id[0]),
    ["3", "4"],
  );

  // With one, a pasted token is needed, and nothing happens without it.
  const reply = await provisionDns({ env: envOf(), fetch: w.fetch }, GRANT);
  const id = JSON.parse(reply.settingsJson).api_token_id as string;
  const minted = { api_token: "minted-1" };
  w.records.push({ id: "5".repeat(32), ...ours });
  w.calls.length = 0;
  const err = await refusal(() =>
    deprovisionDns({ env: envOf({ api_token_id: id }, minted), fetch: w.fetch }, NONE),
  );
  assert.equal(err.wireCode, "unauthorized");
  assert.deepEqual(w.calls, []);
  await deprovisionDns({ env: envOf({ api_token_id: id }, minted), fetch: w.fetch }, GRANT);
  assert.deepEqual(
    w.records.map((r) => r.id[0]),
    ["3", "4"],
  );
  assert.ok(!w.tokens.has("minted-1"));
  // Again: the token is already gone, which is fine.
  await deprovisionDns({ env: envOf({ api_token_id: id }, minted), fetch: w.fetch }, GRANT);
});

test("removal without a host name or a token, and refused removals", async () => {
  const w = world();
  // Nothing to use: nothing happens.
  await deprovisionDns({ env: envOf(), fetch: w.fetch }, NONE);
  assert.deepEqual(w.calls, []);
  // A bad host name with no minted token: nothing to find.
  await deprovisionDns(
    { env: envOf({ hostname: "" }, { api_token: STORED }), fetch: w.fetch },
    NONE,
  );
  assert.deepEqual(w.calls, []);
  // A bad host name with a minted token: the account is found from the token.
  await provisionDns({ env: envOf(), fetch: w.fetch }, GRANT);
  await deprovisionDns(
    { env: envOf({ hostname: "", api_token_id: "a".repeat(31) + "1" }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(!w.tokens.has("minted-1"));
  assert.ok(w.calls.some((call: string) => call === "GET /accounts (pasted)"));

  // A zone without its account: the minted token cannot be deleted.
  w.zoneAccount = false;
  const noAccount = await refusal(() =>
    deprovisionDns({ env: envOf({ api_token_id: "b".repeat(32) }), fetch: w.fetch }, GRANT),
  );
  assert.match(noAccount.message, /did not say which account owns/);
  w.zoneAccount = true;
  // A failing delete is reported.
  w.deleteFails = true;
  const failed = await refusal(() =>
    deprovisionDns({ env: envOf({ api_token_id: "b".repeat(32) }), fetch: w.fetch }, GRANT),
  );
  assert.equal(failed.wireCode, "unavailable");
  assert.match(failed.message, /could not delete the token/);
  // A malformed stored id is refused rather than sent.
  w.deleteFails = false;
  const malformed = await refusal(() =>
    deprovisionDns({ env: envOf({ api_token_id: "../x" }), fetch: w.fetch }, GRANT),
  );
  assert.match(malformed.message, /not 32 lowercase hex digits/);
  // A stored token Cloudflare no longer accepts.
  const stale = await refusal(() =>
    deprovisionDns({ env: envOf({}, { api_token: "revoked" }), fetch: w.fetch }, NONE),
  );
  assert.match(stale.message, /Zone: DNS: Edit/);
});

test("the account: given, the only one, or refused", async () => {
  const w = world();
  const call = async (path: string): Promise<never> => assert.fail(`no call expected: ${path}`);
  assert.equal(await findAccount(call, ACCOUNT), ACCOUNT);
  await assert.rejects(findAccount(call, "nope"), (err: unknown) => {
    assert.ok(err instanceof SetupError);
    assert.equal(err.code, "invalid_params");
    return true;
  });
  /**
   * Lists the given accounts.
   *
   * @param ids - Account ids.
   * @returns A call answering the list.
   */
  const lister = (ids: unknown) => async () => ({ ok: true as const, status: 200, result: ids });
  await assert.rejects(findAccount(lister([]), ""), /no Cloudflare account/);
  await assert.rejects(
    findAccount(lister([{ id: ACCOUNT }, { id: "f".repeat(32) }, null, { id: 1 }]), ""),
    /more than one account/,
  );
  await assert.rejects(findAccount(lister({}), ""), /not a list/);
  await assert.rejects(
    findAccount(async () => ({ ok: false as const, status: 403, message: "" }), ""),
    (err: unknown) =>
      err instanceof SetupError && err.code === "forbidden" && /answered 403$/.test(err.message),
  );
  void w;
});

test("grants: kind none and blank tokens carry none", () => {
  assert.equal(grantToken({ kind: "none", token: "x" }), null);
  assert.equal(grantToken({ kind: "apiToken" }), null);
  assert.equal(grantToken({ kind: "oauthAccessToken", token: " t " }), "t");
});

test("the provisioning class calls the setup", async () => {
  const w = world();
  const original = globalThis.fetch;
  globalThis.fetch = w.fetch as typeof fetch;
  try {
    const entry = new Provisioning({} as never, envOf() as never);
    const reply = await entry.provision(GRANT);
    assert.deepEqual(reply.secrets, [{ key: "api_token", value: "minted-1" }]);
    await entry.deprovision(NONE);
  } finally {
    globalThis.fetch = original;
  }
});

test("an error that is not Cloudflare's goes up as it is", async () => {
  /**
   * Answers with a body that breaks.
   *
   * @returns The response.
   */
  const broken = async (): Promise<Response> =>
    new Response(
      new ReadableStream({
        pull(controller) {
          controller.error(new RangeError("stream broke"));
        },
      }),
    );
  await assert.rejects(provisionDns({ env: envOf(), fetch: broken }, GRANT), RangeError);
});
