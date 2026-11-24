// Exercises the v3 author model (`BookclerkEntrypoint`, `EventMessage`,
// `JobController`, named entrypoints) against the built `dist/` without a
// workerd runtime. `cloudflare:workers` is stubbed through a module hook.
import assert from "node:assert/strict";
import { register } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const STUB = `
export class WorkerEntrypoint {
  constructor(ctx, env) { this.ctx = ctx; this.env = env; }
}
export class RpcTarget {}
`;
const hooks = `
export function resolve(specifier, context, next) {
  if (specifier === "cloudflare:workers") return { url: "stub:cloudflare-workers", shortCircuit: true };
  return next(specifier, context);
}
export function load(url, context, next) {
  if (url === "stub:cloudflare-workers") {
    return { format: "module", source: ${JSON.stringify(STUB)}, shortCircuit: true };
  }
  return next(url, context);
}
`;
register(`data:text/javascript,${encodeURIComponent(hooks)}`, pathToFileURL(root + "/"));

const {
  BookclerkEntrypoint,
  CliEntrypoint,
  EventBatch,
  EventMessage,
  JobController,
  PluginError,
  QueueBatch,
  QueueMessage,
  PURCHASE_HINTS_CONCURRENCY,
  StorefrontEntrypoint,
  cliArgs,
  decodeExtensibleConfig,
  eventBatchResults,
  jobOutcomeFor,
  jsonPayload,
  queueBatchResults,
  scheduledControllerOf,
} = await import(join(dist, "plugin.js"));

const enc = new TextEncoder();

function wireEvent(overrides = {}) {
  return {
    eventId: "evt-1",
    eventType: "book_acquired",
    schemaVersion: 2,
    occurredAtUnixMs: 1_700_000_000_000,
    deliveryAttempt: 3,
    accountId: "acct",
    source: "echo",
    correlationId: "corr",
    causationId: "cause",
    deduplicationKey: "dedup",
    payload: enc.encode(JSON.stringify({ asin: "B00" })),
    ...overrides,
  };
}

// --- EventMessage --------------------------------------------------------

{
  const m = new EventMessage(wireEvent());
  assert.equal(m.id, "evt-1");
  assert.equal(m.type, "book_acquired");
  assert.equal(m.schemaVersion, 2);
  assert.equal(m.timestamp.getTime(), 1_700_000_000_000);
  assert.equal(m.attempts, 3);
  assert.deepEqual(m.json(), { asin: "B00" });
  assert.equal(m.result, null);
  m.retry({ delaySeconds: 30, reason: "later" });
  m.ack();
  assert.equal(m.result.kind, "retry", "first outcome wins");
  assert.equal(m.result.reason, "later");
  assert.ok(m.result.retryAtUnixMs > Date.now() + 20_000);
}

{
  const m = new EventMessage(wireEvent({ payload: undefined }));
  assert.deepEqual(m.json(), {}, "empty body decodes to {}");
  m.suspend({
    checkpoint: { page: 2 },
    wakeAt: new Date(1_800_000_000_000),
    wakeOnEventType: "scan_finished",
    wakeOnFilter: { accountId: "acct" },
  });
  assert.deepEqual(m.result, {
    kind: "suspended",
    checkpointJson: '{"page":2}',
    checkpointSchemaVersion: 1,
    wakeAtUnixMs: 1_800_000_000_000,
    wakeOnEventType: "scan_finished",
    wakeOnFilterJson: '{"accountId":"acct"}',
  });
}

{
  const m = new EventMessage(wireEvent());
  assert.throws(
    () => m.suspend({ checkpoint: "x".repeat(1 << 20) }),
    (err) => err instanceof PluginError && err.wireCode === "payload_too_large",
    "oversized checkpoints are rejected before they reach the wire",
  );
  assert.equal(m.result, null);
}

{
  const m = new EventMessage(
    wireEvent({ resumePending: true, checkpointJson: '{"page":1}', checkpointSchemaVersion: 4 }),
  );
  assert.equal(m.resumePending, true);
  assert.deepEqual(m.checkpoint, { json: '{"page":1}', schemaVersion: 4 });
}

// --- EventBatch + eventBatchResults --------------------------------------

{
  const batch = new EventBatch([
    wireEvent(),
    wireEvent({ eventId: "evt-2" }),
    wireEvent({ eventId: "evt-3" }),
  ]);
  assert.equal(batch.type, "book_acquired");
  batch.messages[0].reject("bad");
  batch.messages[1].deadLetter("park");
  assert.deepEqual(eventBatchResults(batch, undefined), [
    { kind: "reject", reason: "bad" },
    { kind: "deadLetter", reason: "park" },
    { kind: "ack" },
  ]);
  assert.deepEqual(eventBatchResults(batch, new Error("boom"))[2], {
    kind: "retry",
    retryAtUnixMs: 0,
    reason: "boom",
  });
  const mixed = new EventBatch([wireEvent(), wireEvent({ eventType: "other" })]);
  assert.equal(mixed.type, "", "mixed batches report an empty shared type");
  mixed.retryAll({ reason: "all" });
  assert.ok(mixed.messages.every((m) => m.result.kind === "retry"));
}

// --- BookclerkEntrypoint.bookclerkEvent -----------------------------------

{
  const seen = [];
  class Plugin extends BookclerkEntrypoint {
    async event(batch) {
      seen.push(this.env.CONFIG.greeting, this.env.SECRETS.token, this.env.EVENTS, this.env.DB);
      for (const msg of batch.messages) {
        if (msg.id === "evt-2") msg.retry({ reason: "again" });
        else msg.ack();
      }
    }
  }
  // `EVENTS` arrives as the adapter's settled transport: failures come back
  // as values and the author side rebuilds the typed error.
  const settled = [];
  const events = {
    settle: async (method, args) => {
      settled.push([method, args]);
      if (args[0]?.eventType === "denied") {
        return { err: { code: "forbidden", message: "not granted", details: '{"type":"denied"}' } };
      }
      return { ok: { eventId: "evt-1", duplicate: false } };
    },
  };
  const db = { execute: async () => {} };
  const plugin = new Plugin({}, { FIXED: 1 });
  const context = {
    invocation: {
      id: "inv-1",
      accountId: "acct",
      deadlineUnixMs: 0,
      correlationId: "",
      causationId: "",
    },
    config: jsonPayload({ greeting: "hi" }),
    secrets: jsonPayload({ token: "s3cr3t" }),
    events,
    databases: [{ name: "DB", database: db }],
  };
  const results = await plugin.bookclerkEvent(context, {
    events: [wireEvent(), wireEvent({ eventId: "evt-2" })],
  });
  assert.deepEqual(results, [
    { kind: "ack" },
    { kind: "retry", retryAtUnixMs: 0, reason: "again" },
  ]);
  assert.deepEqual(seen.slice(0, 2), ["hi", "s3cr3t"]);
  assert.equal(seen[3], db);
  const EVENTS = seen[2];
  assert.deepEqual(Object.keys(EVENTS).sort(), ["publish", "read"]);
  assert.deepEqual(await EVENTS.publish({ eventType: "ok" }), {
    eventId: "evt-1",
    duplicate: false,
  });
  await assert.rejects(
    () => EVENTS.publish({ eventType: "denied" }),
    (err) =>
      err instanceof PluginError &&
      err.wireCode === "forbidden" &&
      err.message === "not granted" &&
      err.details?.type === "denied",
    "an err reply becomes a typed PluginError with its code and details",
  );
  assert.deepEqual(settled[0], ["publish", [{ eventType: "ok" }]]);
  assert.equal(plugin.env.FIXED, 1, "static env survives the merge");

  const noHandler = new BookclerkEntrypoint({}, {});
  await assert.rejects(
    () => noHandler.bookclerkEvent(context, { events: [] }),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );

  class Throws extends BookclerkEntrypoint {
    async event(batch) {
      batch.messages[0].ack();
      throw new Error("handler exploded");
    }
  }
  const thrown = await new Throws({}, {}).bookclerkEvent(context, {
    events: [wireEvent(), wireEvent({ eventId: "evt-2" })],
  });
  assert.deepEqual(thrown, [
    { kind: "ack" },
    { kind: "retry", retryAtUnixMs: 0, reason: "handler exploded" },
  ]);
}

// --- QueueMessage / QueueBatch / queueBatchResults ------------------------

function wireQueueMessage(overrides = {}) {
  return {
    id: "msg-1",
    timestampUnixMs: 1_700_000_000_000,
    body: enc.encode('{"n":1}'),
    contentType: "json",
    attempts: 2,
    source: "echo",
    ...overrides,
  };
}

{
  const m = new QueueMessage(wireQueueMessage());
  assert.equal(m.id, "msg-1");
  assert.equal(m.timestamp.getTime(), 1_700_000_000_000);
  assert.equal(m.contentType, "json");
  assert.equal(m.attempts, 2);
  assert.equal(m.source, "echo");
  assert.deepEqual(m.json(), { n: 1 });
  assert.equal(m.text(), '{"n":1}');
  assert.equal(m.result, null);
  m.retry({ delaySeconds: 2.9, reason: "later" });
  m.ack();
  assert.deepEqual(
    m.result,
    { kind: "retry", delaySeconds: 2, reason: "later" },
    "first outcome wins",
  );

  const empty = new QueueMessage(wireQueueMessage({ body: undefined, contentType: "" }));
  assert.equal(empty.body.byteLength, 0);
  assert.equal(empty.contentType, "json", "empty content type defaults to json");
  assert.equal(empty.json(), null);

  const batch = new QueueBatch("jobs", [
    wireQueueMessage(),
    wireQueueMessage({ id: "msg-2" }),
    wireQueueMessage({ id: "msg-3" }),
  ]);
  assert.equal(batch.queue, "jobs");
  assert.equal(batch.messages.length, 3);
  batch.messages[0].retry({ reason: "again" });
  assert.deepEqual(queueBatchResults(batch, undefined), [
    { kind: "retry", delaySeconds: 0, reason: "again" },
    { kind: "ack" },
    { kind: "ack" },
  ]);
  assert.deepEqual(queueBatchResults(batch, new Error("boom"))[2], {
    kind: "retry",
    delaySeconds: 0,
    reason: "boom",
  });
  const all = new QueueBatch("jobs", [wireQueueMessage(), wireQueueMessage({ id: "msg-2" })]);
  all.ackAll();
  assert.ok(all.messages.every((m) => m.result.kind === "ack"));
}

// --- BookclerkEntrypoint.bookclerkQueue -----------------------------------

{
  const seen = [];
  class Plugin extends BookclerkEntrypoint {
    async queue(batch) {
      seen.push(batch.queue, batch.invocation.id, this.env.NOTIFY);
      for (const msg of batch.messages) {
        if (msg.id === "msg-2") msg.retry({ delaySeconds: 5, reason: "again" });
        else msg.ack();
      }
    }
  }
  const notify = { settle: async () => ({ ok: { messageIds: ["m-1"] } }) };
  const plugin = new Plugin({}, { FIXED: 1 });
  const context = {
    invocation: {
      id: "inv-q",
      accountId: "",
      deadlineUnixMs: 0,
      correlationId: "",
      causationId: "",
    },
    queues: [{ name: "NOTIFY", queue: notify }],
  };
  const results = await plugin.bookclerkQueue(context, {
    queue: "jobs",
    messages: [wireQueueMessage(), wireQueueMessage({ id: "msg-2" })],
  });
  assert.deepEqual(results, [{ kind: "ack" }, { kind: "retry", delaySeconds: 5, reason: "again" }]);
  assert.deepEqual(seen.slice(0, 2), ["jobs", "inv-q"]);
  assert.deepEqual(await seen[2].send({ n: 1 }), { messageIds: ["m-1"] });
  assert.equal(plugin.env.FIXED, 1, "static env survives the merge");

  await assert.rejects(
    () => new BookclerkEntrypoint({}, {}).bookclerkQueue(context, { queue: "jobs", messages: [] }),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );

  class Throws extends BookclerkEntrypoint {
    async queue(batch) {
      batch.messages[0].ack();
      throw new Error("handler exploded");
    }
  }
  const thrown = await new Throws({}, {}).bookclerkQueue(context, {
    queue: "jobs",
    messages: [wireQueueMessage(), wireQueueMessage({ id: "msg-2" })],
  });
  assert.deepEqual(thrown, [
    { kind: "ack" },
    { kind: "retry", delaySeconds: 0, reason: "handler exploded" },
  ]);
}

// --- BookclerkEntrypoint.bookclerkScheduled -------------------------------

{
  const seen = [];
  class Plugin extends BookclerkEntrypoint {
    async scheduled(controller) {
      seen.push(controller, this.env.FIXED);
    }
  }
  const context = {
    invocation: {
      id: "inv-s",
      accountId: "",
      deadlineUnixMs: 0,
      correlationId: "",
      causationId: "",
    },
  };
  await new Plugin({}, { FIXED: 1 }).bookclerkScheduled(context, {
    cron: "0 3 * * MON",
    scheduledTime: 120_000,
    fireId: "fire-1",
    attempt: 1,
    deadlineUnixMs: 180_000,
  });
  assert.deepEqual(seen, [
    {
      cron: "0 3 * * MON",
      scheduledTime: 120_000,
      fireId: "fire-1",
      attempt: 1,
      deadlineUnixMs: 180_000,
    },
    1,
  ]);
  assert.deepEqual(scheduledControllerOf(undefined), {
    cron: "",
    scheduledTime: 0,
    fireId: "",
    attempt: 0,
    deadlineUnixMs: 0,
  });
  await assert.rejects(
    () => new BookclerkEntrypoint({}, {}).bookclerkScheduled(context, {}),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );
}

// --- BookclerkEntrypoint socket handlers and the SOCKETS helpers ----------

{
  const sockets = await import(join(dist, "sockets.js"));
  const context = { invocation: { id: "inv-ws" } };
  // No webSocketClose: a close is acknowledged and ignored.
  await new BookclerkEntrypoint({}, {}).bookclerkSocketClosed(context, {
    socket: "g1_a",
    code: 1000,
  });
  // No webSocketMessage: the host hears `unsupported`.
  await assert.rejects(
    () => new BookclerkEntrypoint({}, {}).bookclerkSocketMessage(context, { socket: "g1_a" }),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );
  // A handle without a granted transport fails as `unavailable`, not a crash.
  let handle;
  class Plugin extends BookclerkEntrypoint {
    async webSocketMessage(ws) {
      handle = ws;
    }
  }
  await new Plugin({}, {}).bookclerkSocketMessage(context, {
    socket: "g1_a",
    frame: { kind: "text", value: "hi" },
  });
  await assert.rejects(
    () => handle.send("x"),
    (err) => err instanceof PluginError && err.wireCode === "unavailable",
  );

  // Attachments are JSON, bounded, and tolerate another SDK's raw bytes.
  assert.deepEqual(sockets.encodeAttachment(null), new Uint8Array());
  assert.deepEqual(sockets.decodeAttachment(sockets.encodeAttachment({ a: [1] })), { a: [1] });
  assert.equal(sockets.decodeAttachment(new TextEncoder().encode("not json")), "not json");
  assert.throws(
    () => sockets.encodeAttachment("x".repeat(2048)),
    (err) => err.wireCode === "payload_too_large",
  );
  assert.throws(
    () => sockets.encodeAttachment(() => 1),
    (err) => err.wireCode === "invalid_params",
  );
  const cyclic = {};
  cyclic.self = cyclic;
  assert.throws(
    () => sockets.encodeAttachment(cyclic),
    (err) => err.wireCode === "invalid_params",
  );

  // Frames: text stays text, views are sent without their surrounding buffer.
  assert.deepEqual(sockets.routeFrame("hi"), { text: "hi" });
  const view = new Uint8Array([0, 1, 2, 3]).subarray(1, 3);
  assert.deepEqual(sockets.routeFrame(view), { binary: "AQI=" });
  assert.deepEqual(sockets.routeFrame(new Uint8Array([7]).buffer), { binary: "Bw==" });
  assert.throws(
    () => sockets.routeFrame(new Uint8Array(262_145)),
    (err) => err.wireCode === "payload_too_large",
  );
  assert.throws(
    () => sockets.routeFrame(42),
    (err) => err.wireCode === "invalid_params",
  );
  // getWebSocket makes a handle by id without asking the host.
  const asked = [];
  const binding = sockets.socketsBinding({
    async call(op, body) {
      asked.push({ op, body });
      return op === "send" ? { err: { code: "not_found", message: "gone" } } : { ok: {} };
    },
  });
  const byId = binding.getWebSocket("o7");
  assert.equal(asked.length, 0);
  assert.equal(byId.id, "o7");
  assert.equal(byId.url, "");
  assert.equal(byId.deserializeAttachment(), null);
  await byId.close(1000, "bye");
  assert.deepEqual(asked, [{ op: "close", body: { socket: "o7", code: 1000, reason: "bye" } }]);
  await assert.rejects(
    () => byId.send("x"),
    (err) => err instanceof PluginError && err.wireCode === "not_found",
  );
  // A tls:// connect sends trust anchors as base64 DER, from bytes or PEM.
  const connects = [];
  const tlsBinding = sockets.socketsBinding({
    async call(op, body) {
      connects.push(body);
      return { ok: { socket: "s1", url: body.url, protocol: "x-tunnel" } };
    },
  });
  const opened = await tlsBinding.connect("tls://connect.example.com:443", {
    protocols: ["x-tunnel"],
    trustAnchors: [
      new Uint8Array([0x30, 3, 2, 1, 0]),
      "junk\n-----BEGIN CERTIFICATE-----\nMAMC\nAQA=\n-----END CERTIFICATE-----\n",
    ],
  });
  assert.equal(opened.protocol, "x-tunnel");
  assert.deepEqual(connects[0].trustAnchors, ["MAMCAQA=", "MAMCAQA="]);
  assert.equal(connects[0].serverName, "");
  await tlsBinding.connect("tls://region1.v2.argotunnel.com:7844", {
    protocols: ["h2"],
    serverName: "h2.cftunnel.com",
  });
  assert.equal(connects[1].serverName, "h2.cftunnel.com");
  await assert.rejects(
    () => tlsBinding.connect("tls://connect.example.com:443", { trustAnchors: ["no pem"] }),
    (err) => err instanceof PluginError && err.wireCode === "invalid_params",
  );
  assert.equal(sockets.socketMessageData(undefined), "");
  assert.equal(sockets.socketMessageData({ kind: "text", value: "t" }), "t");
}

// --- JobController + jobOutcomeFor ----------------------------------------

const jobInvocation = {
  invocationId: "job-1",
  commandType: "echo.copy",
  payloadJson: '{"n":1}',
  payloadSchemaVersion: 1,
  idempotencyKey: "idem",
  attempt: 2,
  deadlineUnixMs: 0,
  correlationId: "corr",
  causationId: "cause",
  invocationSequence: 0,
  stepId: "",
};

{
  const job = new JobController(jobInvocation);
  assert.equal(job.id, "job-1");
  assert.equal(job.type, "echo.copy");
  assert.deepEqual(job.json(), { n: 1 });
  assert.equal(job.attempt, 2);
  assert.equal(job.input, null);
  assert.equal(job.signal.aborted, false);
  assert.deepEqual(jobOutcomeFor(job, { message: "done", bytesCopied: 7 }, undefined), {
    kind: "completed",
    message: "done",
    bytesCopied: 7,
  });
  assert.deepEqual(jobOutcomeFor(job, undefined, undefined), {
    kind: "completed",
    message: "",
    bytesCopied: 0,
  });
  assert.deepEqual(jobOutcomeFor(job, undefined, new Error("nope")), {
    kind: "rejected",
    message: "nope",
  });
  assert.equal(
    jobOutcomeFor(job, undefined, PluginError.fromWire("unavailable", "down")).kind,
    "retryable",
  );
  assert.equal(
    jobOutcomeFor(job, undefined, PluginError.fromWire("deadline_exceeded", "slow")).kind,
    "retryable",
  );
  assert.equal(
    jobOutcomeFor(job, undefined, PluginError.fromWire("cancelled", "stop")).kind,
    "cancelled",
  );
}

{
  const job = new JobController(jobInvocation);
  job.suspend({
    checkpoint: { cursor: "abc" },
    checkpointSchemaVersion: 3,
    wakeAt: 1_900_000_000_000,
  });
  job.retryLater({ reason: "ignored" });
  assert.deepEqual(jobOutcomeFor(job, undefined, undefined), {
    kind: "suspended",
    checkpoint: { schemaVersion: 3, json: '{"cursor":"abc"}' },
    wakeAtUnixMs: 1_900_000_000_000,
  });
  const retry = new JobController(jobInvocation);
  retry.retryLater({ retryAt: new Date(1_900_000_000_000), reason: "busy" });
  assert.deepEqual(retry.result, {
    kind: "retryable",
    message: "busy",
    retryAfterUnixMs: 1_900_000_000_000,
  });
}

{
  let resolveCancel;
  const cancel = { wait: () => new Promise((r) => (resolveCancel = r)) };
  const reports = [];
  const progress = { report: async (p, m) => reports.push([p, m]) };
  const job = new JobController(jobInvocation, { cancel, progress });
  await job.progress(50, "half");
  assert.deepEqual(reports, [[50, "half"]]);
  const aborted = new Promise((r) => job.signal.addEventListener("abort", r));
  resolveCancel();
  await aborted;
  assert.equal(job.signal.aborted, true);
  assert.equal(jobOutcomeFor(job, undefined, new Error("interrupted")).kind, "cancelled");
}

{
  class Worker extends BookclerkEntrypoint {
    async job(job) {
      await job.progress(10);
      return { message: `ran ${job.type} for ${this.env.CONFIG.who}` };
    }
  }
  const out = await new Worker({}, {}).bookclerkJob(
    { invocation: { id: "inv" }, config: jsonPayload({ who: "acct" }) },
    jobInvocation,
    {},
  );
  assert.deepEqual(out, { kind: "completed", message: "ran echo.copy for acct", bytesCopied: 0 });
  const missing = new BookclerkEntrypoint({}, {});
  await assert.rejects(
    () => missing.bookclerkJob({ invocation: {} }, jobInvocation, {}),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );
}

// --- Named entrypoints ---------------------------------------------------

{
  class Cli extends CliEntrypoint {
    async describe() {
      return { command: "echo", summary: "Echo", args: [] };
    }
    async invoke(params) {
      const args = cliArgs(params);
      return {
        exitCode: 0,
        stdout: `${this.invocation.id}:${args.name}:${this.env.CONFIG.greeting}`,
        stderr: "",
        payload: jsonPayload({ echoed: args.name }),
      };
    }
    hidden() {
      return "nope";
    }
  }
  const cli = new Cli({}, {});
  const context = { invocation: { id: "inv-cli" }, config: jsonPayload({ greeting: "yo" }) };
  const result = await cli.bookclerkInvoke(context, "invoke", {
    command: "echo",
    args: [{ name: "name", value: "world" }],
  });
  assert.equal(result.exitCode, 0);
  assert.equal(result.stdout, "inv-cli:world:yo");
  assert.deepEqual(decodeExtensibleConfig(result.payload), { echoed: "world" });
  await assert.rejects(
    () => cli.bookclerkInvoke(context, "hidden"),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
    "methods outside the static allowlist are not reachable",
  );
  await assert.rejects(
    () => cli.bookclerkInvoke(context, "bookclerkInvoke"),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
  );
  assert.ok(StorefrontEntrypoint.bookclerkMethods.includes("login"));
  const bare = new StorefrontEntrypoint({}, {});
  assert.deepEqual(await bare.bookclerkInvoke(context, "health"), { ok: true, detail: "" });
  await assert.rejects(
    () => bare.bookclerkInvoke(context, "login", { accountId: "a" }),
    (err) => err instanceof PluginError && err.wireCode === "unsupported",
    "declared-but-unimplemented methods report unsupported",
  );

  // The default `purchaseHints` overlaps a few one-title lookups and keeps
  // each answer in its query's place.
  class SlowStore extends StorefrontEntrypoint {
    running = 0;
    peak = 0;
    async purchaseHint(params) {
      this.running += 1;
      this.peak = Math.max(this.peak, this.running);
      await new Promise((resolve) => setTimeout(resolve, 10));
      this.running -= 1;
      return params.productId ? { productId: params.productId } : null;
    }
  }
  const slow = new SlowStore({}, {});
  const queries = Array.from({ length: 10 }, (_, n) =>
    n % 3 === 0 ? { title: `t${n}` } : { productId: `p${n}` },
  );
  const hints = await slow.purchaseHints({ queries });
  assert.deepEqual(
    hints.map((h) => h?.productId ?? null),
    queries.map((q) => q.productId ?? null),
  );
  assert.equal(slow.peak, PURCHASE_HINTS_CONCURRENCY);
  class LimitedStore extends StorefrontEntrypoint {
    async purchaseHint() {
      throw new PluginError("rate_limited", "slow down");
    }
  }
  await assert.rejects(
    () => new LimitedStore({}, {}).purchaseHints({ queries }),
    (err) => err instanceof PluginError && err.wireCode === "rate_limited",
    "a failed lookup rejects the whole batch",
  );
  // After one lane fails, the lanes still waiting finish but start nothing new.
  class OneFailsStore extends StorefrontEntrypoint {
    asked = 0;
    async purchaseHint(params) {
      this.asked += 1;
      if (params.productId === "p1") throw new PluginError("rate_limited", "slow down");
      await new Promise((resolve) => setTimeout(resolve, 10));
      return { productId: params.productId };
    }
  }
  const oneFails = new OneFailsStore({}, {});
  const named = Array.from({ length: 10 }, (_, n) => ({ productId: `p${n}` }));
  await assert.rejects(() => oneFails.purchaseHints({ queries: named }));
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(oneFails.asked, PURCHASE_HINTS_CONCURRENCY, "no lookup starts after a failure");
}

// --- Native-behind-workerd control plane (dist + bundled embed) ---

const embed = await import(join(root, "../embed/bookclerk_plugin.js"));
const distAdapter = await import(join(dist, "plugin.js"));

for (const [label, mod] of [
  ["dist", distAdapter],
  ["embed", embed],
]) {
  const { allowedEntrypointFamilies, wrapPluginFromNative, PluginError: Err } = mod;
  assert.equal(typeof allowedEntrypointFamilies, "function", `${label} exports the filter`);
  assert.equal(mod.HttpNativeRoot, undefined, `${label} has no HTTP native data plane`);

  const storageOnly = { entrypoints: ["storage"], consumes: [], jobs: [] };
  assert.deepEqual(
    allowedEntrypointFamilies(storageOnly, [
      "eventConsumer",
      "jobRunner",
      "storage",
      "cli",
      "databaseAdapter",
      "bogus",
    ]),
    ["jobRunner", "storage"],
    `${label}: storage guests keep jobRunner (stream_copy) and lose undeclared families`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: ["cli"], consumes: [], jobs: [] }, [
      "jobRunner",
      "storage",
      "cli",
    ]),
    ["cli"],
    `${label}: cli-only manifest nulls storage and jobRunner`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies(
      { entrypoints: [], consumes: [{ type: "book_acquired" }], jobs: [] },
      ["eventConsumer", "jobRunner", "storage", "storage"],
    ),
    ["eventConsumer"],
    `${label}: eventConsumer requires consumers; duplicates collapse`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: [], consumes: [], jobs: ["stream_copy"] }, [
      "jobRunner",
    ]),
    ["jobRunner"],
    `${label}: declared jobs enable jobRunner`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies(null, ["storage"]),
    [],
    `${label}: missing manifest fails closed`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: [], consumes: [], jobs: [], sockets: true }, [
      "socketHandler",
      "socketHandler",
    ]),
    ["socketHandler"],
    `${label}: [sockets] enables socketHandler; duplicates collapse`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: [], consumes: [], jobs: [], sockets: false }, [
      "socketHandler",
    ]),
    [],
    `${label}: socketHandler requires [sockets]`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies(
      { entrypoints: [], consumes: [], jobs: [], queueConsumers: ["jobs"] },
      ["queueConsumer", "eventConsumer", "queueConsumer"],
    ),
    ["queueConsumer"],
    `${label}: queueConsumer requires [[queues.consumers]]; duplicates collapse`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: ["cli"], consumes: [], jobs: [] }, [
      "queueConsumer",
      "cli",
    ]),
    ["cli"],
    `${label}: no queue consumers nulls queueConsumer`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: [], consumes: [], jobs: [], crons: ["0 3 * * *"] }, [
      "scheduledHandler",
      "jobRunner",
    ]),
    ["scheduledHandler"],
    `${label}: scheduledHandler requires [triggers] crons`,
  );
  assert.deepEqual(
    allowedEntrypointFamilies({ entrypoints: ["cli"], consumes: [], jobs: [] }, [
      "scheduledHandler",
    ]),
    [],
    `${label}: no crons nulls scheduledHandler`,
  );

  const Adapter = wrapPluginFromNative();
  const manifest = {
    apiVersion: 3,
    id: "sqlite",
    name: "SQLite",
    version: "0.1.0",
    capabilities: { entrypoints: ["databaseAdapter"], consumes: [], jobs: [], databases: [] },
    cli: { commands: [{ name: "vacuum", summary: "", args: [] }] },
  };
  const adapter = new Adapter({}, { PLUGIN_DESCRIBE: JSON.stringify(manifest), BRIDGE_TOKEN: "t" });

  assert.deepEqual(
    await adapter.openInvocation({ invocation: { id: "inv-1" } }, [
      "databaseAdapter",
      "storage",
      "jobRunner",
    ]),
    ["databaseAdapter"],
    `${label}: openInvocation keeps only manifest-declared families`,
  );
  await assert.rejects(
    () => adapter.openInvocation({ invocation: { id: "" } }, ["databaseAdapter"]),
    (err) => err instanceof Err && err.wireCode === "invalid_params",
    `${label}: openInvocation rejects an empty invocation id`,
  );
  await assert.rejects(
    () => adapter.openInvocation(undefined, ["databaseAdapter"]),
    (err) => err instanceof Err && err.wireCode === "invalid_params",
    `${label}: openInvocation rejects a missing envelope`,
  );

  const merged = await adapter.describe({
    apiVersion: 3,
    id: "guest-says-otherwise",
    name: "Guest Name",
    version: "9.9.9",
    capabilities: { entrypoints: ["storage"], consumes: [], jobs: [], databases: [] },
    features: ["rpcFeature"],
    limits: { maxBatch: 7 },
    cli: { commands: [] },
  });
  assert.equal(merged.id, "sqlite", `${label}: manifest id wins`);
  assert.deepEqual(
    merged.capabilities.entrypoints,
    ["databaseAdapter"],
    `${label}: manifest capabilities win`,
  );
  assert.equal(merged.name, "Guest Name", `${label}: guest presentation is kept`);
  assert.equal(merged.version, "9.9.9", `${label}: guest version is kept`);
  assert.deepEqual(merged.features, ["rpcFeature"], `${label}: guest features are kept`);
  assert.deepEqual(merged.limits, { maxBatch: 7 }, `${label}: guest limits are kept`);
  assert.equal(
    merged.cli.commands[0].name,
    "vacuum",
    `${label}: an empty guest cli keeps the manifest cli`,
  );
  const guestCli = await adapter.describe({
    cli: { commands: [{ name: "compact", summary: "", args: [] }] },
  });
  assert.equal(
    guestCli.cli.commands[0].name,
    "compact",
    `${label}: a guest cli with commands wins`,
  );
  assert.equal(
    guestCli.id,
    "sqlite",
    `${label}: partial guest describe still takes manifest identity`,
  );
  await assert.rejects(
    () => adapter.describe(null),
    (err) => err instanceof Err && err.wireCode === "invalid_params",
    `${label}: describe(null) is rejected`,
  );
  await adapter.shutdown();
}

// --- ORIGIN binding and the granted /origin/fetch framing ---------------

{
  const origin = await import(join(dist, "origin.js"));
  const enc = new TextEncoder();
  const streamOf = (...chunks) =>
    new ReadableStream({
      start(controller) {
        for (const chunk of chunks) controller.enqueue(chunk);
        controller.close();
      },
    });
  const readAll = async (stream) => new Uint8Array(await new Response(stream).arrayBuffer());

  // The binding sends the origin-form target and drops connection and
  // forwarding headers; the client address comes only from the options.
  let seen;
  const binding = origin.originBinding({
    async fetch(head, body) {
      seen = { head, body: body ? new TextDecoder().decode(await readAll(body)) : null };
      return {
        ok: {
          status: 201,
          headers: [
            { name: "set-cookie", value: "a=1" },
            { name: "set-cookie", value: "b=2" },
          ],
          body: streamOf(enc.encode("done")),
        },
      };
    },
  });
  const resp = await binding.fetch(
    new Request("https://lib.example/api/status?x=1", {
      method: "POST",
      headers: {
        "content-type": "text/plain",
        host: "evil.example",
        "x-forwarded-for": "1.2.3.4",
        connection: "keep-alive",
      },
      body: "hello",
    }),
    { clientAddress: "203.0.113.9" },
  );
  assert.equal(seen.head.method, "POST");
  assert.equal(seen.head.target, "/api/status?x=1");
  assert.equal(seen.head.clientAddress, "203.0.113.9");
  assert.deepEqual(
    seen.head.headers.map((h) => h.name),
    ["content-type"],
    "host, forwarding and connection headers are dropped",
  );
  assert.equal(seen.body, "hello");
  assert.equal(resp.status, 201);
  assert.deepEqual(resp.headers.getSetCookie(), ["a=1", "b=2"]);
  assert.equal(await resp.text(), "done");

  // A transport failure surfaces as the typed PluginError.
  const refusing = origin.originBinding({
    fetch: async () => ({ err: { code: "unavailable", message: "not now" } }),
  });
  await assert.rejects(
    () => refusing.fetch(new Request("https://lib.example/")),
    (err) => err instanceof PluginError && err.wireCode === "unavailable",
  );

  // The granted call frames the head ahead of the body and reads the framed
  // response head back, however the reply is chunked.
  const head = { status: 200, headers: [{ name: "x-a", value: "1" }] };
  const reply = new Uint8Array([...origin.encodeOriginFrame(head), ...enc.encode("world")]);
  const granted = {
    async fetch(url, init) {
      assert.equal(url, "http://granted/origin/fetch");
      assert.equal(init.headers.Authorization, "Bearer otok");
      const sent = await readAll(init.body);
      const len = new DataView(sent.buffer).getUint32(0, false);
      const request = JSON.parse(new TextDecoder().decode(sent.subarray(4, 4 + len)));
      assert.equal(request.target, "/x");
      assert.equal(new TextDecoder().decode(sent.subarray(4 + len)), "ping");
      // Split mid-prefix and mid-head.
      return new Response(streamOf(reply.slice(0, 2), reply.slice(2, 9), reply.slice(9)), {
        status: 200,
      });
    },
  };
  const ok = await origin.grantedOriginFetch(
    granted,
    { Authorization: "Bearer otok" },
    { method: "PUT", target: "/x", headers: [] },
    streamOf(enc.encode("pi"), enc.encode("ng")),
  );
  assert.equal(ok.ok.status, 200);
  assert.deepEqual(ok.ok.headers, head.headers);
  assert.equal(new TextDecoder().decode(await readAll(ok.ok.body)), "world");

  // A plugin error from the host keeps its code.
  const refused = await origin.grantedOriginFetch(
    {
      fetch: async () =>
        new Response(JSON.stringify({ error: { code: "invalid_params", message: "bad" } }), {
          status: 400,
        }),
    },
    {},
    { method: "GET", target: "/", headers: [] },
    null,
  );
  assert.deepEqual(refused, { err: { code: "invalid_params", message: "bad" } });

  // A reply that ends inside its head, or claims an oversized head, is internal.
  const truncated = await origin.grantedOriginFetch(
    { fetch: async () => new Response(streamOf(reply.slice(0, 6)), { status: 200 }) },
    {},
    { method: "GET", target: "/", headers: [] },
    null,
  );
  assert.equal(truncated.err.code, "internal");
  const oversized = await origin.grantedOriginFetch(
    {
      fetch: async () =>
        new Response(streamOf(new Uint8Array([0xff, 0xff, 0xff, 0xff])), { status: 200 }),
    },
    {},
    { method: "GET", target: "/", headers: [] },
    null,
  );
  assert.equal(oversized.err.code, "internal");
  // A channel that cannot be reached is unavailable.
  const unreachable = await origin.grantedOriginFetch(
    {
      fetch: async () => {
        throw new Error("down");
      },
    },
    {},
    { method: "GET", target: "/", headers: [] },
    null,
  );
  assert.equal(unreachable.err.code, "unavailable");

  // An upgrade sends the head without handshake fields and the attachment
  // as JSON; an accepted one is a socket of the SOCKETS grant.
  let connectSeen;
  const socketCalls = [];
  const upgrading = origin.originBinding(
    {
      async connect(head, attachment) {
        connectSeen = { head, attachment };
        if (head.target === "/refuse") {
          return {
            ok: {
              refused: {
                status: 401,
                headers: [{ name: "content-type", value: "text/plain" }],
                body: btoa("sign in"),
              },
            },
          };
        }
        return {
          ok: {
            socket: {
              socket: "o1",
              url: "https://lib.example/api/live",
              protocol: "bookclerk.v1",
              attachment: attachment,
            },
          },
        };
      },
    },
    {
      async call(op, body) {
        socketCalls.push([op, body]);
        return { ok: {} };
      },
    },
  );
  const accepted = await upgrading.connect(
    new Request("https://lib.example/api/live?t=1", {
      headers: {
        upgrade: "websocket",
        connection: "Upgrade",
        "sec-websocket-protocol": "bookclerk.v1",
        cookie: "s=1",
      },
    }),
    { clientAddress: "203.0.113.9", attachment: { relay: "r1" } },
  );
  assert.equal(connectSeen.head.method, "GET");
  assert.equal(connectSeen.head.target, "/api/live?t=1");
  assert.equal(connectSeen.head.clientAddress, "203.0.113.9");
  assert.deepEqual(
    connectSeen.head.headers.map((h) => h.name),
    ["cookie", "sec-websocket-protocol"],
    "connection fields are dropped",
  );
  assert.deepEqual(JSON.parse(atob(connectSeen.attachment)), { relay: "r1" });
  assert.equal(accepted.socket.id, "o1");
  assert.equal(accepted.socket.protocol, "bookclerk.v1");
  assert.deepEqual(accepted.socket.deserializeAttachment(), { relay: "r1" });
  await accepted.socket.send("hi");
  assert.deepEqual(socketCalls, [["send", { socket: "o1", frame: { text: "hi" } }]]);

  const refusedUpgrade = await upgrading.connect(new Request("https://lib.example/refuse"));
  assert.equal(refusedUpgrade.response.status, 401);
  assert.equal(await refusedUpgrade.response.text(), "sign in");

  await assert.rejects(
    () =>
      origin
        .originBinding({ connect: async () => ({ ok: { neither: true } }) })
        .connect(new Request("https://lib.example/")),
    (err) => err instanceof PluginError && err.wireCode === "internal",
  );
  await assert.rejects(
    () =>
      origin
        .originBinding({ connect: async () => ({ err: { code: "forbidden", message: "no" } }) })
        .connect(new Request("https://lib.example/")),
    (err) => err instanceof PluginError && err.wireCode === "forbidden",
  );

  // The granted call posts JSON and hands back the route's answer.
  const connectOk = await origin.grantedOriginConnect(
    {
      async fetch(url, init) {
        assert.equal(url, "http://granted/origin/connect");
        assert.equal(init.headers.Authorization, "Bearer otok");
        const body = JSON.parse(init.body);
        assert.equal(body.request.target, "/live");
        assert.equal(body.attachment, "e30=");
        return Response.json({ socket: { socket: "o2", url: "u", attachment: "" } });
      },
    },
    { Authorization: "Bearer otok" },
    { method: "GET", target: "/live", headers: [] },
    "e30=",
  );
  assert.equal(connectOk.ok.socket.socket, "o2");
  const connectRefused = await origin.grantedOriginConnect(
    {
      fetch: async () =>
        new Response(JSON.stringify({ error: { code: "limit_exceeded", message: "full" } }), {
          status: 429,
        }),
    },
    {},
    { method: "GET", target: "/", headers: [] },
    "",
  );
  assert.deepEqual(connectRefused, { err: { code: "limit_exceeded", message: "full" } });
  const connectDown = await origin.grantedOriginConnect(
    {
      fetch: async () => {
        throw new Error("down");
      },
    },
    {},
    { method: "GET", target: "/", headers: [] },
    "",
  );
  assert.equal(connectDown.err.code, "unavailable");
}

console.log("v3 entrypoint translation ok");
