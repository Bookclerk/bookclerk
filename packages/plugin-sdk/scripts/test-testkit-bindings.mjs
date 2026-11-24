/**
 * The testkit's in-memory `KV` and `WEBHOOKS` bindings hold the host's
 * limits and error codes. The Python testkit's `TestKv` and `TestWebhooks`
 * are checked against the same cases.
 */
import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const dist = join(dirname(fileURLToPath(import.meta.url)), "../dist");
const { createTestKv, createTestWebhooks } = await import(join(dist, "testkit.js"));
const { PluginError } = await import(join(dist, "errors.js"));

/**
 * Asserts `run` rejects with a plugin error of `code`.
 *
 * @param {Function} run - The call.
 * @param {string} code - The expected code.
 */
async function rejectsWith(run, code) {
  await assert.rejects(
    async () => run(),
    (err) => err instanceof PluginError && err.code === code,
  );
}

// KV: values, metadata, expiry, listing and limits.
{
  let clock = 1_000_000;
  const kv = createTestKv({ maxValueBytes: 16, maxKeys: 3, now: () => clock });
  assert.equal(await kv.get("missing"), null);
  await kv.put("a", "hello", { metadata: { n: 1 } });
  assert.equal(await kv.get("a"), "hello");
  assert.deepEqual(await kv.getWithMetadata("a"), { value: "hello", metadata: { n: 1 } });
  await kv.put("j", JSON.stringify({ x: [1] }));
  assert.deepEqual(await kv.get("j", "json"), { x: [1] });
  assert.deepEqual(
    new Uint8Array(await kv.get("a", "arrayBuffer")),
    new TextEncoder().encode("hello"),
  );
  await kv.put("t", new Uint8Array([1, 2]), { expirationTtl: 10 });
  assert.notEqual(await kv.get("t"), null);
  await rejectsWith(() => kv.put("four", "x"), "forbidden");
  await kv.put("a", "again");
  clock += 10_000;
  assert.equal(await kv.get("t"), null, "expired keys read as absent");
  await kv.put("four", "x");
  await rejectsWith(() => kv.put("", "x"), "invalid_params");
  await rejectsWith(() => kv.put("k".repeat(513), "x"), "payload_too_large");
  await rejectsWith(() => kv.get("k".repeat(513)), "payload_too_large");
  await rejectsWith(() => kv.put("a", "x".repeat(17)), "payload_too_large");
  await rejectsWith(() => kv.put("a", "x", { metadata: "m".repeat(1100) }), "payload_too_large");
  await rejectsWith(() => kv.put("a", "x", { expirationTtl: -1 }), "invalid_params");
  const first = await kv.list({ limit: 2 });
  assert.deepEqual(
    first.keys.map((k) => k.name),
    ["a", "four"],
  );
  assert.equal(first.list_complete, false);
  const rest = await kv.list({ cursor: first.cursor });
  assert.deepEqual(rest, { keys: [{ name: "j" }], list_complete: true });
  assert.deepEqual((await kv.list({ prefix: "f" })).keys, [{ name: "four" }]);
  await rejectsWith(() => kv.list({ limit: 257 }), "invalid_params");
  await kv.delete("j");
  await kv.delete("j");
  assert.deepEqual([...kv.snapshot().keys()], ["a", "four"]);
  kv.failNext("get", PluginError.fromWire("unavailable", "down"));
  await rejectsWith(() => kv.get("a"), "unavailable");
  assert.equal(await kv.get("a"), "again", "a failure fires once");
  // The namespace cap never passes the ABI's.
  const big = createTestKv({ maxValueBytes: 10 * 1024 * 1024 });
  await rejectsWith(() => big.put("k", new Uint8Array(1024 * 1024 + 1)), "payload_too_large");
}

// WEBHOOKS: leases, acks, retries, missed and limits.
{
  const hooks = createTestWebhooks();
  assert.equal(await hooks.url("changes"), "https://library.example/hooks/changes");
  assert.equal(await createTestWebhooks({ baseUrl: null }).url("changes"), null);
  hooks.deliver("changes", "one", { "x-sig": "s" });
  hooks.deliver("changes", new Uint8Array([2]));
  hooks.deliver("other", "elsewhere");
  const first = await hooks.pull("changes", { batchSize: 1 });
  assert.equal(first.missed, false);
  assert.equal(first.messages.length, 1);
  const [one] = first.messages;
  assert.equal(new TextDecoder().decode(one.body), "one");
  assert.deepEqual(one.headers, { "x-sig": "s" });
  assert.equal(one.attempts, 1);
  assert.ok(one.timestamp instanceof Date);
  assert.equal(hooks.waiting("changes"), 1);
  await hooks.ack("changes", { retries: [one.leaseId] });
  assert.deepEqual(hooks.retried("changes"), [one.leaseId]);
  const again = await hooks.pull("changes");
  assert.equal(again.messages.length, 2);
  assert.equal(again.messages[0].attempts, 2, "a retried request comes back first");
  await hooks.ack("other", { acks: [again.messages[0].leaseId] });
  assert.deepEqual(hooks.acked("other"), [], "a lease settles only under its own name");
  await hooks.ack("changes", { acks: again.messages.map((m) => m.leaseId) });
  assert.equal(hooks.acked("changes").length, 2);
  assert.equal(hooks.waiting("changes"), 0);
  hooks.markMissed("changes");
  assert.equal((await hooks.pull("changes")).missed, true);
  assert.equal((await hooks.pull("changes")).missed, false);
  await rejectsWith(() => hooks.pull("changes", { batchSize: 0 }), "invalid_params");
  await rejectsWith(() => hooks.pull("changes", { batchSize: 101 }), "invalid_params");
  await rejectsWith(
    () => hooks.pull("changes", { visibilityTimeoutMs: 43_200_001 }),
    "invalid_params",
  );
}

console.log("testkit-bindings: ok");
