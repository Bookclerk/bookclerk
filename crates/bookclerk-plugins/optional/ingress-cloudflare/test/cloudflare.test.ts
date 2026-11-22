/**
 * Deploying the relay through a fake Cloudflare API.
 */

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  API_BASE,
  DeployError,
  MIGRATION_TAG,
  deployRelay,
  isAccountId,
  isDnsLabel,
  relayHost,
  relayToken,
  uploadMetadata,
  type Fetch,
  type RelayTarget,
} from "../src/cloudflare.ts";
import { RELAY_MODULES } from "../src/relay-source.ts";

/** The deploy target in these tests. */
const TARGET: RelayTarget = {
  accountId: "0123456789abcdef0123456789abcdef",
  workerName: "bookclerk-relay",
  apiToken: "cf-api-token",
};

/** One call the fake API saw. */
interface Call {
  /** Method. */
  method: string;
  /** Path under the API base. */
  path: string;
  /** Authorization header. */
  auth: string | null;
  /** Request body. */
  body: BodyInit | null | undefined;
}

/**
 * A fake Cloudflare API.
 *
 * @param scripts - The account's Workers, as the list returns them.
 * @param overrides - Answers by `METHOD path`, replacing the defaults.
 * @returns The fetch and the calls it saw.
 */
function fakeApi(
  scripts: unknown[],
  overrides: Record<string, () => Response> = {},
): { fetch: Fetch; calls: Call[] } {
  const calls: Call[] = [];
  const ok = (result: unknown): Response => Response.json({ success: true, errors: [], result });
  const fetch: Fetch = async (input, init) => {
    assert.ok(input.startsWith(API_BASE), input);
    const path = input.slice(API_BASE.length);
    const method = init?.method ?? "GET";
    calls.push({
      method,
      path,
      auth: new Headers(init?.headers).get("authorization"),
      body: init?.body,
    });
    const override = overrides[`${method} ${path}`];
    if (override) return override();
    const account = `/accounts/${TARGET.accountId}`;
    if (method === "GET" && path === `${account}/workers/scripts`) return ok(scripts);
    if (method === "PUT" && path === `${account}/workers/scripts/${TARGET.workerName}`)
      return ok({ id: TARGET.workerName });
    if (method === "POST" && path === `${account}/workers/scripts/${TARGET.workerName}/subdomain`)
      return ok({ enabled: true });
    if (method === "GET" && path === `${account}/workers/subdomain`)
      return ok({ subdomain: "acme" });
    return new Response(
      JSON.stringify({ success: false, errors: [{ code: 7003, message: "no route" }] }),
      { status: 404 },
    );
  };
  return { fetch, calls };
}

/**
 * The metadata part of an upload.
 *
 * @param body - The upload's body.
 * @returns The parsed metadata.
 */
async function metadataOf(body: BodyInit | null | undefined): Promise<Record<string, unknown>> {
  assert.ok(body instanceof FormData);
  const part = body.get("metadata");
  assert.ok(part instanceof Blob);
  return JSON.parse(await part.text()) as Record<string, unknown>;
}

test("a first deploy creates the Durable Object and returns the workers.dev host", async () => {
  const { fetch, calls } = fakeApi([{ id: "other", migration_tag: "" }]);
  const host = await deployRelay(fetch, TARGET, "relay-token-0123456789");
  assert.equal(host, "bookclerk-relay.acme.workers.dev");
  assert.deepEqual(
    calls.map((c) => `${c.method} ${c.path.replace(`/accounts/${TARGET.accountId}`, "")}`),
    [
      "GET /workers/scripts",
      "PUT /workers/scripts/bookclerk-relay",
      "POST /workers/scripts/bookclerk-relay/subdomain",
      "GET /workers/subdomain",
    ],
  );
  assert.ok(calls.every((c) => c.auth === "Bearer cf-api-token"));
  const upload = calls[1];
  assert.ok(upload);
  const metadata = await metadataOf(upload.body);
  assert.deepEqual(metadata["migrations"], {
    new_tag: MIGRATION_TAG,
    new_sqlite_classes: ["Relay"],
  });
  assert.equal(metadata["main_module"], "relay-worker.js");
  assert.ok(upload.body instanceof FormData);
  for (const module of RELAY_MODULES)
    assert.ok(upload.body.get(module.name) instanceof Blob, module.name);
});

test("a redeploy of the Bookclerk relay keeps its Durable Object", async () => {
  const { fetch, calls } = fakeApi([{ id: TARGET.workerName, migration_tag: MIGRATION_TAG }]);
  await deployRelay(fetch, TARGET, "relay-token-0123456789");
  const metadata = await metadataOf(calls[1]?.body);
  assert.equal(metadata["migrations"], undefined);
});

test("a Worker of the same name that Bookclerk did not create is refused", async () => {
  const { fetch } = fakeApi([{ id: TARGET.workerName }]);
  await assert.rejects(deployRelay(fetch, TARGET, "t"), (err: unknown) => {
    assert.ok(err instanceof DeployError);
    assert.match(err.message, /already exists/);
    return true;
  });
});

test("a refused token names the permission it needs and never echoes the token", async () => {
  const { fetch } = fakeApi([], {
    [`GET /accounts/${TARGET.accountId}/workers/scripts`]: () =>
      new Response(
        JSON.stringify({
          success: false,
          errors: [{ code: 10000, message: "Authentication error" }],
        }),
        {
          status: 403,
        },
      ),
  });
  await assert.rejects(deployRelay(fetch, TARGET, "t"), (err: unknown) => {
    assert.ok(err instanceof DeployError);
    assert.match(err.message, /Workers Scripts: Edit/);
    assert.doesNotMatch(err.message, /cf-api-token/);
    return true;
  });
});

test("an account without a workers.dev subdomain says how to get one", async () => {
  const { fetch } = fakeApi([], {
    [`GET /accounts/${TARGET.accountId}/workers/subdomain`]: () =>
      Response.json({ success: true, errors: [], result: { subdomain: "" } }),
  });
  await assert.rejects(deployRelay(fetch, TARGET, "t"), /no workers.dev subdomain/);
});

test("odd API answers are reported, not trusted", async () => {
  const account = `/accounts/${TARGET.accountId}`;
  const cases: Array<[string, () => Response, RegExp]> = [
    [
      `GET ${account}/workers/scripts`,
      () => new Response("<html>", { status: 502 }),
      /without JSON/,
    ],
    [`GET ${account}/workers/scripts`, () => Response.json(null), /without an API envelope/],
    [
      `GET ${account}/workers/scripts`,
      () => Response.json({ success: true, result: {} }),
      /not a list/,
    ],
    [
      `GET ${account}/workers/scripts`,
      () => Response.json({ success: false }, { status: 500 }),
      /answered 500$/,
    ],
    [
      `GET ${account}/workers/subdomain`,
      () => Response.json({ success: true, result: { subdomain: "Bad_Label" } }),
      /unusable workers.dev subdomain/,
    ],
  ];
  for (const [key, answer, expected] of cases) {
    const { fetch } = fakeApi([], { [key]: answer });
    await assert.rejects(deployRelay(fetch, TARGET, "t"), expected, key);
  }
  await assert.rejects(
    deployRelay(() => Promise.reject(new Error("offline")), TARGET, "t"),
    /could not reach the Cloudflare API/,
  );
});

test("bad settings are refused before any call", async () => {
  const { fetch, calls } = fakeApi([]);
  await assert.rejects(deployRelay(fetch, { ...TARGET, accountId: "nope" }, "t"), /account_id/);
  await assert.rejects(
    deployRelay(fetch, { ...TARGET, workerName: "Bad Name" }, "t"),
    /worker_name/,
  );
  assert.equal(calls.length, 0);
});

test("names, tokens and metadata", async () => {
  assert.equal(isAccountId(TARGET.accountId), true);
  assert.equal(isAccountId(TARGET.accountId.toUpperCase()), false);
  assert.equal(isDnsLabel("a"), true);
  assert.equal(isDnsLabel("-a"), false);
  assert.equal(isDnsLabel("xn--abc"), false);
  assert.equal(isDnsLabel("a".repeat(64)), false);
  assert.throws(() => relayHost("Bad", "acme"), DeployError);
  const token = await relayToken(TARGET);
  assert.match(token, /^[A-Za-z0-9_-]{43}$/);
  assert.equal(await relayToken(TARGET), token);
  assert.notEqual(await relayToken({ ...TARGET, apiToken: "other" }), token);
  assert.notEqual(await relayToken({ ...TARGET, workerName: "other" }), token);
  const metadata = uploadMetadata(token, false);
  assert.deepEqual(metadata["bindings"], [
    { type: "durable_object_namespace", name: "RELAY", class_name: "Relay" },
    { type: "secret_text", name: "RELAY_TOKEN", text: token },
  ]);
});

test("the embedded relay is the emitted Worker", () => {
  assert.deepEqual(
    RELAY_MODULES.map((m) => m.name),
    ["relay-worker.js", "relay-core.js", "protocol.js"],
  );
  const main = RELAY_MODULES[0];
  assert.ok(main);
  assert.match(main.source, /export class Relay/);
  assert.match(main.source, /from "\.\/relay-core\.js"/);
});
