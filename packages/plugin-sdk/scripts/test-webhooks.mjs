import assert from "node:assert/strict";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const { webhooksBinding, checkWebhookName } = await import(join(dist, "webhooks.js"));
const { MAX_WEBHOOK_PULL_BATCH, MAX_WEBHOOK_VISIBILITY_MS } = await import(join(dist, "abi.js"));

/**
 * A transport that records calls and answers from `replies`.
 *
 * @param {Record<string, object>} replies - Reply per operation.
 * @returns {{calls: Array<[string, object]>, call: Function}} The transport.
 */
function transport(replies) {
  const calls = [];
  return {
    calls,
    async call(op, body) {
      calls.push([op, body]);
      return replies[op] ?? { ok: {} };
    },
  };
}

/**
 * Asserts `promise` rejects with a `PluginError` carrying `code`.
 *
 * @param {Promise<unknown>} promise - The call.
 * @param {string} code - Expected wire code.
 * @returns {Promise<void>} Resolves once checked.
 */
async function rejectsWith(promise, code) {
  await assert.rejects(promise, (err) => err.code === code);
}

// url: a string, or null when the host hands none out.
{
  const t = transport({ url: { ok: { url: "https://lib.example.org/hooks/t/events" } } });
  const hooks = webhooksBinding(t);
  assert.equal(await hooks.url("events"), "https://lib.example.org/hooks/t/events");
  assert.deepEqual(t.calls, [["url", { name: "events" }]]);
  assert.equal(
    await webhooksBinding(transport({ url: { ok: { url: null } } })).url("events"),
    null,
  );
}

// pull: messages decoded, defaults sent, missed passed through.
{
  const t = transport({
    pull: {
      ok: {
        deliveries: [
          {
            leaseId: "l1",
            receivedAtUnixMs: 1000,
            attempt: 2,
            headers: [{ name: "content-type", value: "application/json" }],
            body: "e30=",
          },
        ],
        missed: true,
      },
    },
  });
  const page = await webhooksBinding(t).pull("events");
  assert.equal(page.missed, true);
  assert.equal(page.messages.length, 1);
  const [message] = page.messages;
  assert.equal(message.leaseId, "l1");
  assert.equal(message.attempts, 2);
  assert.equal(message.timestamp.getTime(), 1000);
  assert.deepEqual(message.headers, { "content-type": "application/json" });
  assert.deepEqual([...message.body], [0x7b, 0x7d]);
  assert.ok(Object.isFrozen(message));
  assert.deepEqual(t.calls, [["pull", { name: "events", batchSize: 10, visibilityTimeoutMs: 0 }]]);
}

// pull options are checked before any call.
{
  const t = transport({});
  const hooks = webhooksBinding(t);
  await rejectsWith(hooks.pull("events", { batchSize: 0 }), "invalid_params");
  await rejectsWith(
    hooks.pull("events", { batchSize: MAX_WEBHOOK_PULL_BATCH + 1 }),
    "invalid_params",
  );
  await rejectsWith(
    hooks.pull("events", { visibilityTimeoutMs: MAX_WEBHOOK_VISIBILITY_MS + 1 }),
    "invalid_params",
  );
  await rejectsWith(hooks.pull("Events"), "invalid_params");
  assert.equal(t.calls.length, 0);
}

// ack: lists checked, overlap refused, host errors keep their code.
{
  const t = transport({});
  const hooks = webhooksBinding(t);
  await hooks.ack("events", { acks: ["l1"], retries: ["l2"] });
  assert.deepEqual(t.calls, [["ack", { name: "events", acks: ["l1"], retries: ["l2"] }]]);
  await rejectsWith(hooks.ack("events", { acks: ["x"], retries: ["x"] }), "invalid_params");
  await rejectsWith(hooks.ack("events", { acks: ["no/slash"] }), "invalid_params");
  await rejectsWith(
    hooks.ack("events", {
      acks: Array.from({ length: MAX_WEBHOOK_PULL_BATCH + 1 }, (_, i) => `l${i}`),
    }),
    "invalid_params",
  );
  const failing = webhooksBinding(
    transport({ ack: { err: { code: "not_found", message: "gone" } } }),
  );
  await rejectsWith(failing.ack("events", { acks: ["l1"] }), "not_found");
}

// Names follow the host's rule.
assert.equal(checkWebhookName("events-2"), "events-2");
for (const bad of ["", "-x", "A", "a_b", "x".repeat(33)]) {
  assert.throws(
    () => checkWebhookName(bad),
    (err) => err.code === "invalid_params",
  );
}

console.log("webhooks binding: ok");
