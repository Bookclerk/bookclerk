/**
 * The guided setup and its removal, with the SDK's isolate module replaced
 * by stand-ins and a fake Cloudflare API (accounts, Workers and account API
 * tokens) behind `fetch`.
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
  namedExports: {
    BookclerkEntrypoint: Entrypoint,
    IngressEntrypoint: Entrypoint,
    ProvisioningEntrypoint: Entrypoint,
    PluginError,
    provisioned,
  },
});
const { Provisioning } = await import("../src/index.ts");
const { accountResource, deprovisionRelay, provisionRelay, tokenName } =
  await import("../src/provision.ts");
const { MAX_ANSWER_BYTES } = await import("../src/api.ts");
const { API_BASE, MIGRATION_TAG, parseScriptState } = await import("../src/cloudflare.ts");
const { SetupError, tokenRequest } = await import("../src/tokens.ts");

/** The fake account's id. */
const ACCOUNT = "3f4b7e3dcab231cbfdaa90a6a28bd548";

/** The token the operator pastes: it may do everything. */
const PASTED = "pasted-token";

/** The token stored before the setup ran: Workers only. */
const STORED = "stored-token";

/** The Workers Scripts Write group's id. */
const GROUP = "c".repeat(32);

/** A Worker in the fake account. */
interface FakeScript {
  /** Name. */
  id: string;
  /** Migration tag. */
  migration_tag?: string;
}

/** The fake world behind `fetch`. */
interface World {
  /** The account's Workers. */
  scripts: FakeScript[];
  /** Live account token ids by value. */
  tokens: Map<string, string>;
  /** Bodies of token create calls. */
  minted: Record<string, unknown>[];
  /** Calls, as `METHOD path (token)`. */
  calls: string[];
  /** The accounts `GET /accounts` lists. */
  accounts: string[];
  /** Whether the group is offered. */
  offered: boolean;
  /** Makes Worker deletes fail with this status. */
  scriptDeleteStatus: number;
  /** Replaces every answer with this body. */
  raw: string | null;
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
    scripts: [],
    tokens: new Map([[STORED, "e".repeat(32)]]),
    minted: [],
    calls: [],
    accounts: [ACCOUNT],
    offered: true,
    scriptDeleteStatus: 0,
    raw: null,
    fetch: async (input, init) => {
      assert.ok(input.startsWith(API_BASE), input);
      assert.equal(init?.redirect, "manual");
      if (w.raw !== null) return new Response(w.raw);
      const url = new URL(input);
      const path = url.pathname.slice(new URL(API_BASE).pathname.length);
      const method = init?.method ?? "GET";
      const token = (new Headers(init?.headers).get("authorization") ?? "").replace(/^Bearer /, "");
      w.calls.push(`${method} ${path} (${token === PASTED ? "pasted" : token})`);
      const pasted = token === PASTED;
      if (!pasted && !w.tokens.has(token)) return refused(401, "Invalid API Token");
      const scripts = `/accounts/${ACCOUNT}/workers/scripts`;
      if (path === scripts) return json(200, { success: true, result: w.scripts });
      const script = w.scripts.find((s) => path === `${scripts}/${s.id}`);
      if (script && method === "DELETE") {
        if (w.scriptDeleteStatus) return refused(w.scriptDeleteStatus, "No");
        w.scripts.splice(w.scripts.indexOf(script), 1);
        return json(200, { success: true, result: null });
      }
      if (!pasted) return refused(403, "Unauthorized to access requested resource");
      if (path === "/accounts") {
        return json(200, { success: true, result: w.accounts.map((id) => ({ id })) });
      }
      const tokens = `/accounts/${ACCOUNT}/tokens`;
      if (path === `${tokens}/permission_groups`) {
        const name = url.searchParams.get("name") ?? "";
        const result = w.offered && name === "Workers Scripts Write" ? [{ id: GROUP, name }] : [];
        return json(200, { success: true, result });
      }
      if (path === tokens && method === "POST") {
        const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
        w.minted.push(body);
        const id = (++next).toString(16).padStart(32, "a");
        const value = `minted-${next}`;
        w.tokens.set(value, id);
        return json(200, { success: true, result: { id, value } });
      }
      if (path.startsWith(`${tokens}/`) && method === "DELETE") {
        const id = path.slice(tokens.length + 1);
        if (id === "b".repeat(32)) return refused(500, "Internal error");
        const entry = [...w.tokens].find(([, tokenId]) => tokenId === id);
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
): Parameters<typeof provisionRelay>[0]["env"] {
  return {
    CONFIG: { account_id: "", worker_name: "bookclerk-relay", ...config },
    SECRETS: secrets,
  } as unknown as Parameters<typeof provisionRelay>[0]["env"];
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

test("the setup finds the account and mints a Workers token for it", async () => {
  const w = world();
  const reply = await provisionRelay({ env: envOf(), fetch: w.fetch }, GRANT);
  assert.deepEqual(JSON.parse(reply.settingsJson), {
    account_id: ACCOUNT,
    api_token_id: "a".repeat(31) + "1",
  });
  assert.deepEqual(reply.secrets, [{ key: "api_token", value: "minted-1" }]);
  assert.deepEqual(reply.summary, [
    "The relay Worker will be named bookclerk-relay",
    "Created an API token that can deploy Workers in this account only",
    "The relay is deployed when the ingress starts",
  ]);
  assert.deepEqual(w.minted, [
    tokenRequest(tokenName("bookclerk-relay"), [GROUP], accountResource(ACCOUNT)),
  ]);
  assert.deepEqual(w.minted[0], {
    name: "Bookclerk cloudflare_relay bookclerk-relay",
    policies: [
      {
        effect: "allow",
        permission_groups: [{ id: GROUP }],
        resources: { [`com.cloudflare.api.account.${ACCOUNT}`]: "*" },
      },
    ],
  });
  assert.ok(
    w.calls.every((call) => call.endsWith("(pasted)")),
    w.calls.join(", "),
  );
  assert.ok(!JSON.stringify(reply).includes(PASTED));
});

test("a second setup finds the relay and rotates the token", async () => {
  const w = world();
  w.scripts.push({ id: "my-relay", migration_tag: MIGRATION_TAG });
  const config = { account_id: ACCOUNT, worker_name: "my-relay" };
  const first = await provisionRelay({ env: envOf(config), fetch: w.fetch }, GRANT);
  assert.equal(first.summary[0], "Found your relay Worker my-relay");
  // The given account is used without listing accounts.
  assert.ok(!w.calls.some((call) => call.startsWith("GET /accounts ")));
  const id = JSON.parse(first.settingsJson).api_token_id as string;
  const second = await provisionRelay(
    { env: envOf({ ...config, api_token_id: id }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(second.summary.includes("Deleted the token an earlier setup created"));
  assert.ok(!w.tokens.has("minted-1"));
  const third = await provisionRelay(
    { env: envOf({ ...config, api_token_id: id }), fetch: w.fetch },
    GRANT,
  );
  assert.equal(third.summary.length, 3);
  const fourth = await provisionRelay(
    { env: envOf({ ...config, api_token_id: "b".repeat(32) }), fetch: w.fetch },
    GRANT,
  );
  assert.ok(fourth.summary.some((line) => line.startsWith("Could not delete the earlier token")));
});

test("the setup refuses a taken name, bad choices and a missing grant", async () => {
  const w = world();
  const ctx = (config: Record<string, unknown> = {}): Parameters<typeof provisionRelay>[0] => ({
    env: envOf(config),
    fetch: w.fetch,
  });
  assert.equal(
    (await refusal(() => provisionRelay(ctx(), { grant: NONE.grant, inputsJson: "{}" }))).wireCode,
    "invalid_params",
  );
  for (const bad of [{ worker_name: "Bad_Name" }, { account_id: "nope" }]) {
    assert.equal((await refusal(() => provisionRelay(ctx(bad), GRANT))).wireCode, "invalid_params");
  }
  w.scripts.push({ id: "bookclerk-relay" });
  const taken = await refusal(() => provisionRelay(ctx(), GRANT));
  assert.equal(taken.wireCode, "invalid_params");
  assert.match(taken.message, /already exists and is not a Bookclerk relay/);
  // An unset name falls back to the default.
  assert.match(
    (await refusal(() => provisionRelay(ctx({ worker_name: "" }), GRANT))).message,
    /"bookclerk-relay"/,
  );
  w.scripts.length = 0;
  w.offered = false;
  assert.equal((await refusal(() => provisionRelay(ctx(), GRANT))).wireCode, "forbidden");
  w.offered = true;
  w.accounts.push("f".repeat(32));
  assert.match(
    (await refusal(() => provisionRelay(ctx(), GRANT))).message,
    /more than one account/,
  );
  w.accounts.pop();
  const known = { account_id: ACCOUNT };
  w.raw = "not json";
  assert.match((await refusal(() => provisionRelay(ctx(known), GRANT))).message, /without JSON/);
  w.raw = "[]";
  assert.match(
    (await refusal(() => provisionRelay(ctx(known), GRANT))).message,
    /without an API envelope/,
  );
  w.raw = "x".repeat(MAX_ANSWER_BYTES + 1);
  assert.match((await refusal(() => provisionRelay(ctx(known), GRANT))).message, /at length/);
  w.raw = '{"success":true,"result":{}}';
  assert.match(
    (await refusal(() => provisionRelay(ctx(known), GRANT))).message,
    /Worker list is not a list/,
  );
  w.raw = null;
  const offline = await refusal(() =>
    provisionRelay(
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
    provisionRelay(
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

test("removal deletes the relay Worker and the minted token", async () => {
  const w = world();
  w.scripts.push({ id: "bookclerk-relay", migration_tag: MIGRATION_TAG }, { id: "other" });
  // The stored token removes the relay when there is no minted token.
  await deprovisionRelay(
    { env: envOf({ account_id: ACCOUNT }, { api_token: STORED }), fetch: w.fetch },
    NONE,
  );
  assert.deepEqual(w.scripts, [{ id: "other" }]);
  // A Worker that is not a relay is left alone.
  await deprovisionRelay(
    {
      env: envOf({ account_id: ACCOUNT, worker_name: "other" }, { api_token: STORED }),
      fetch: w.fetch,
    },
    NONE,
  );
  assert.deepEqual(w.scripts, [{ id: "other" }]);

  // A minted token needs a pasted one.
  w.scripts.push({ id: "bookclerk-relay", migration_tag: MIGRATION_TAG });
  const reply = await provisionRelay({ env: envOf(), fetch: w.fetch }, GRANT);
  const id = JSON.parse(reply.settingsJson).api_token_id as string;
  w.calls.length = 0;
  const err = await refusal(() =>
    deprovisionRelay(
      { env: envOf({ api_token_id: id }, { api_token: "minted-1" }), fetch: w.fetch },
      NONE,
    ),
  );
  assert.equal(err.wireCode, "unauthorized");
  assert.equal(w.calls.length, 0);
  // Without an account id, the pasted token finds it.
  await deprovisionRelay({ env: envOf({ api_token_id: id }), fetch: w.fetch }, GRANT);
  assert.deepEqual(w.scripts, [{ id: "other" }]);
  assert.ok(!w.tokens.has("minted-1"));

  // Nothing to use: nothing happens.
  w.calls.length = 0;
  await deprovisionRelay({ env: envOf(), fetch: w.fetch }, NONE);
  assert.equal(w.calls.length, 0);

  // A Worker already gone answers 404, which is fine; another refusal is not.
  w.scripts.push({ id: "bookclerk-relay", migration_tag: MIGRATION_TAG });
  w.scriptDeleteStatus = 404;
  await deprovisionRelay(
    { env: envOf({ account_id: ACCOUNT }, { api_token: STORED }), fetch: w.fetch },
    NONE,
  );
  w.scriptDeleteStatus = 403;
  const refusedDelete = await refusal(() =>
    deprovisionRelay(
      { env: envOf({ account_id: ACCOUNT }, { api_token: STORED }), fetch: w.fetch },
      NONE,
    ),
  );
  assert.equal(refusedDelete.wireCode, "forbidden");
  assert.match(refusedDelete.message, /could not delete the relay Worker/);
});

test("Worker lists: absent, a relay, or another Worker", () => {
  assert.equal(parseScriptState([], "a"), "absent");
  assert.equal(parseScriptState([null, { id: "a", migration_tag: MIGRATION_TAG }], "a"), "relay");
  assert.equal(parseScriptState([{ id: "a", migration_tag: "v9" }], "a"), "other");
  assert.throws(() => parseScriptState("x", "a"), SetupError);
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
