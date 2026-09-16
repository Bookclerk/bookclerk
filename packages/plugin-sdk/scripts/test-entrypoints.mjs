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
  StorefrontEntrypoint,
  cliArgs,
  decodeExtensibleConfig,
  eventBatchResults,
  jobOutcomeFor,
  jsonPayload,
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
  const batch = new EventBatch([wireEvent(), wireEvent({ eventId: "evt-2" }), wireEvent({ eventId: "evt-3" })]);
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
  const events = { publish: async () => {} };
  const db = { execute: async () => {} };
  const plugin = new Plugin({}, { FIXED: 1 });
  const context = {
    invocation: { id: "inv-1", accountId: "acct", deadlineUnixMs: 0, correlationId: "", causationId: "" },
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
  assert.deepEqual(seen, ["hi", "s3cr3t", events, db]);
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
  job.suspend({ checkpoint: { cursor: "abc" }, checkpointSchemaVersion: 3, wakeAt: 1_900_000_000_000 });
  job.retryLater({ reason: "ignored" });
  assert.deepEqual(jobOutcomeFor(job, undefined, undefined), {
    kind: "suspended",
    checkpoint: { schemaVersion: 3, json: '{"cursor":"abc"}' },
    wakeAtUnixMs: 1_900_000_000_000,
  });
  const retry = new JobController(jobInvocation);
  retry.retryLater({ retryAt: new Date(1_900_000_000_000), reason: "busy" });
  assert.deepEqual(retry.result, { kind: "retryable", message: "busy", retryAfterUnixMs: 1_900_000_000_000 });
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
}

console.log("v3 entrypoint translation ok");
