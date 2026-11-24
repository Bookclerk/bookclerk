/**
 * The entrypoint glue and the change feed: settings and secrets, the
 * declared features, a staged write and its commit through the `storage`
 * entrypoint, and the feed turning on, delivering and withdrawing its rules.
 */

import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";

import {
  createTestKv,
  createTestWebhooks,
} from "../node_modules/@bookclerk/plugin-sdk/dist/testkit-bindings.js";
import { PluginError } from "./sdk.ts";
import { FakeB2, installDigestStream } from "./fake-b2.ts";

installDigestStream();

const { default: Plugin, Storage, RPC_FEATURES, destinationFrom } = await import("../src/index.ts");
const { B2ChangeFeed, RULE_RECHECK_MS } = await import("../src/feed.ts");
const { B2Store } = await import("../src/backend.ts");
const { B2SigningSecret } = await import("../src/b2-events.ts");

/** UTF-8 bytes. */
const bytes = (text: string): Uint8Array<ArrayBuffer> => new TextEncoder().encode(text);
/** A one-chunk stream. */
const stream = (data: Uint8Array<ArrayBuffer>): ReadableStream<Uint8Array> =>
  new Blob([data]).stream();

/** A valid signing secret. */
const SECRET = "abcdefghijklmnopqrstuvwxyz012345";

let b2: FakeB2;
/** A unique prefix per test, so cached stores do not cross tests. */
let prefix = "";
let counter = 0;

beforeEach(() => {
  b2 = new FakeB2();
  prefix = `t${counter++}/`;
});

/** Bindings for one instance. */
function env(extra: Record<string, unknown> = {}, secrets: Record<string, string> = {}): never {
  return {
    CONFIG: { bucket: "books", prefix },
    SECRETS: { application_key_id: "kid", application_key: "secret", ...secrets },
    KV: createTestKv({ maxValueBytes: 1 << 20 }),
    ...extra,
  } as never;
}

test("bad settings and secrets are refused before anything is sent", () => {
  const refused = (e: unknown, match: RegExp) => {
    assert.throws(
      () => destinationFrom(e as never, b2.fetch),
      (err: unknown) =>
        err instanceof PluginError && err.code === "invalid_params" && match.test(err.message),
    );
  };
  refused({ CONFIG: { bucket: " " }, SECRETS: {} }, /bucket is not set/);
  refused(
    { CONFIG: { bucket: "books" }, SECRETS: { application_key_id: "kid" } },
    /application key/,
  );
  refused(
    {
      CONFIG: { bucket: "books", prefix: "a//b" },
      SECRETS: { application_key_id: "kid", application_key: "k" },
    },
    /prefix/,
  );
  refused(
    {
      CONFIG: { bucket: "books" },
      SECRETS: { application_key_id: "kid", application_key: "k", webhook_signing_secret: "short" },
    },
    /signing secret/,
  );
  assert.equal(b2.seen.length, 0);
});

test("describe declares what B2 does natively", async () => {
  const described = await new Plugin({} as never, {} as never).describe();
  assert.equal(described.displayName, "Backblaze B2");
  for (const feature of [
    "rpc.scalarLimits",
    "rpc.streams",
    "storage.copy",
    "storage.translateKeys",
    "storage.checkedWrite",
    "storage.changes",
  ]) {
    assert.ok(described.rpcFeatures?.includes(feature), feature);
  }
  assert.ok(
    !RPC_FEATURES.includes("storage.conditionalWrite"),
    "B2 has no atomic conditional writes",
  );
});

test("a staged write commits through the storage entrypoint", async () => {
  const dest = destinationFrom(env(), b2.fetch);
  const staged = await dest.put("book/a.m4b", stream(bytes("audio")), {
    commitToken: "sc-1",
    stageOnly: true,
    contentType: "audio/mp4",
  });
  assert.equal(staged.bytesWritten, 5);
  assert.equal(b2.current(`${prefix}book/a.m4b`), undefined, "a staged write is not published");
  const committed = await dest.commit("book/a.m4b", "sc-1");
  assert.equal(committed.bytesWritten, 5);
  assert.equal(new TextDecoder().decode(b2.current(`${prefix}book/a.m4b`)?.data), "audio");
  assert.equal(b2.current(`${prefix}book/a.m4b`)?.fileInfo["commit-token"], "sc-1");
  assert.equal(
    [...b2.files.keys()].filter((name) => name.includes(".bookclerk-stage")).length,
    0,
    "the stage is gone",
  );
  // A repeated commit after the stage is gone recognizes the published object.
  const again = await dest.commit("book/a.m4b", "sc-1");
  assert.equal(again.bytesWritten, 5);
  const keys = await dest.translateKeys(["book/a.m4b"]);
  assert.deepEqual(keys, ["book/a.m4b"]);
});

test("the storage entrypoint delegates every call to the destination", async (t) => {
  // The entrypoint sends with the isolate's own `fetch`; point it at the fake.
  t.mock.method(globalThis, "fetch", (input: string, init: RequestInit) => b2.fetch(input, init));
  const storage = new Storage({} as never, env() as never);
  await storage.put("k", stream(bytes("v")), {});
  assert.equal((await storage.head("k"))?.size, 1);
  assert.deepEqual(
    (await storage.list({})).objects.map((o) => o.key),
    ["k"],
  );
  const read = await storage.get("k");
  assert.equal(await new Response(read.body).text(), "v");
  assert.equal((await storage.copy("k", "k2")).bytesCopied, 1);
  await storage.delete("k2");
  assert.equal(await storage.head("k2"), null);
  await storage.put("s", stream(bytes("x")), { commitToken: "t", stageOnly: true });
  await storage.abortStage("s", "t");
  assert.deepEqual(await storage.translateKeys(["a"]), ["a"]);
  const page = await storage.changes({});
  assert.equal(page.complete, false);
  assert.equal(page.cursor, "none");
});

test("without a webhook binding or secret the feed is off and the host lists", async () => {
  const dest = destinationFrom(env({}, { webhook_signing_secret: SECRET }), b2.fetch);
  const page = await dest.changes({});
  assert.equal(page.cursor, "none");
  assert.equal(page.complete, false);
  assert.equal(b2.count("b2_get_bucket_notification_rules"), 0, "no rules were ever written");
});

test("the feed keeps its rules, delivers changes and withdraws the rules", async () => {
  const webhooks = createTestWebhooks();
  const kv = createTestKv({ maxValueBytes: 1 << 20 });
  let now = 1_000_000;
  const store = new B2Store(
    { keyId: "kid", applicationKey: "secret", bucket: "books", prefix: "library/" },
    { fetch: b2.fetch, sleep: () => Promise.resolve() },
  );
  const secret = B2SigningSecret.parse(SECRET);
  assert.ok(secret instanceof B2SigningSecret);
  const feed = new B2ChangeFeed({
    store,
    kv,
    webhooks,
    webhook: "changes",
    secret,
    now: () => now,
  });

  const first = await feed.changes(undefined, 0);
  assert.ok(first);
  assert.equal(first.complete, false, "a page after writing the rules is incomplete");
  assert.equal(b2.rules.length, 3);
  assert.equal(b2.count("b2_set_bucket_notification_rules"), 1);

  webhooks.deliver(
    "changes",
    JSON.stringify({
      events: [
        {
          eventType: "b2:ObjectCreated:Upload",
          bucketName: "books",
          objectName: "library/x.m4b",
          eventTimestamp: 5,
        },
        {
          eventType: "b2:ObjectCreated:Upload",
          bucketName: "books",
          objectName: "elsewhere/y",
          eventTimestamp: 6,
        },
      ],
    }),
  );
  const second = await feed.changes(first.cursor, 0);
  assert.ok(second);
  const third = await feed.changes(second.cursor, 0);
  assert.ok(third);
  const keys = [...second.changes, ...third.changes].map((change) => change.key);
  assert.deepEqual(keys, ["x.m4b"]);
  assert.equal(
    b2.count("b2_get_bucket_notification_rules"),
    1,
    "rules in place are trusted for an hour",
  );

  now += RULE_RECHECK_MS;
  await feed.changes(third.cursor, 0);
  assert.equal(b2.count("b2_get_bucket_notification_rules"), 2, "and checked again after it");
  assert.equal(
    b2.count("b2_set_bucket_notification_rules"),
    1,
    "rules still in place are not rewritten",
  );

  // Rules B2 refuses to set leave the page incomplete instead of failing it.
  b2.rules = [];
  now += RULE_RECHECK_MS;
  b2.fail("b2_set_bucket_notification_rules", 400);
  const refused = await feed.changes(third.cursor, 0);
  assert.equal(refused?.complete, false);

  // The library is no longer reachable: the rules are withdrawn.
  await feed.changes(third.cursor, 0);
  assert.equal(b2.rules.length, 3);
  const offline = new B2ChangeFeed({
    store,
    kv,
    webhooks: createTestWebhooks({ baseUrl: null }),
    webhook: "changes",
    secret,
  });
  assert.equal(await offline.changes(undefined, 0), null);
  assert.equal(b2.rules.length, 0);
  assert.equal(await offline.changes(undefined, 0), null, "nothing left to withdraw");
  assert.equal(b2.count("b2_set_bucket_notification_rules"), 4);

  // An unreadable rule state reads as nothing installed.
  for (const [key] of kv.snapshot())
    if (key.startsWith("b2-rules-")) await kv.put(key, "{not json");
  const off = new B2ChangeFeed({
    store,
    kv,
    webhooks: undefined,
    webhook: "changes",
    secret: undefined,
  });
  assert.equal(await off.changes(undefined, 0), null);
});
