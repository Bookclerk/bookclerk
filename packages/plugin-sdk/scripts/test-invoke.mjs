// Drives the adapter isolate's `POST /invoke` dispatch (`InvocationAdapter.invoke`)
// end-to-end against the built `dist/`: request bytes come from the generated
// `<Iface><Method>ParamsCodec`, replies are decoded with the matching
// `ResultsCodec`. `cloudflare:workers` is stubbed through a module hook and
// the author entrypoints are plain instances standing in for RPC stubs.
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
  AdapterDatabaseSession,
  BookclerkEntrypoint,
  CliEntrypoint,
  DatabaseAdapterEntrypoint,
  PluginError,
  StorageEntrypoint,
  StorefrontEntrypoint,
  dataMigrationOp,
  jsonPayload,
  schemaMigrationOp,
  toBridgeJson,
  wrapPluginFromBinding,
} = await import(join(dist, "plugin.js"));
const { InvokeError, dispatchInvoke } = await import(join(dist, "invoke.js"));
const W = await import(join(dist, "generated-wire.js"));

const enc = new TextEncoder();

// --- Author isolate stand-ins --------------------------------------------

const calls = { event: 0, storefront: [] };

class Plugin extends BookclerkEntrypoint {
  async event(batch) {
    calls.event += 1;
    for (const msg of batch.messages) {
      if (msg.id === "evt-2") msg.retry({ reason: "again" });
      else msg.ack();
    }
  }
  async job(job) {
    await job.progress(50, "half");
    jobHooks.onProgressed?.();
    if (job.type === "cancel-me") {
      await new Promise((resolve) => job.signal.addEventListener("abort", resolve, { once: true }));
      throw new Error("stopped");
    }
    return { message: `ran ${job.type} for ${this.env.CONFIG.who}`, bytesCopied: 9 };
  }
  async databaseMigrations(binding) {
    return [
      {
        id: `${binding}-001`,
        operations: [schemaMigrationOp("CREATE TABLE seen (id TEXT)"), dataMigrationOp("INSERT INTO seen VALUES ('x')")],
      },
    ];
  }
}
const jobHooks = {};

class Storefront extends StorefrontEntrypoint {
  async searchCatalog(params) {
    calls.storefront.push(["searchCatalog", params.query]);
    return [
      { productId: "p1", title: `hit for ${params.query}`, authors: "A" },
      { productId: "p2", title: "second" },
    ];
  }
  async purchaseHint(params) {
    return params.productId === "known" ? { productId: "known", title: "Known" } : null;
  }
  async catalogDetail(params) {
    return params.productId === "known" ? { productId: "known", title: "Known" } : null;
  }
  async diagnose() {
    return [`greeting=${this.env.CONFIG.greeting}`, `secret=${this.env.SECRETS.token}`, `inv=${this.invocation.id}`];
  }
  async listAccounts() {
    return [{ accountId: "a1", source: "echo", marketplace: "us", displayName: "A" }];
  }
}

class Storage extends StorageEntrypoint {
  async head(key) {
    return key === "present" ? { key, size: 3, contentType: "text/plain" } : null;
  }
  async list(options) {
    calls.list = options;
    return { objects: [{ key: "a/1", size: 3 }], nextCursor: "next" };
  }
  async copy(from, to) {
    return { bytesCopied: from.length + to.length };
  }
  async delete() {}
  async commit(key, commitToken) {
    return { key, bytesWritten: 7, etag: commitToken };
  }
  async abortStage() {
    throw PluginError.fromWire("not_found", "no such stage");
  }
}

class Session extends AdapterDatabaseSession {
  async capabilities() {
    return {
      sqlContractVersion: 1,
      atomicBatch: true,
      returning: true,
      affectedRows: true,
      schemaMigrations: false,
    };
  }
  async listUserRelations() {
    return ["books", "authors"];
  }
  async dropUserRelations(names) {
    calls.dropped = names;
  }
}

class DatabaseAdapter extends DatabaseAdapterEntrypoint {
  async openSession() {
    return new Session();
  }
}

class Cli extends CliEntrypoint {
  async describe() {
    return { commands: [{ name: "ping", summary: "Probe", args: [] }] };
  }
}

// --- Granted channel fake -------------------------------------------------

const granted = {
  requests: [],
  cancelState: { polls: 0, release: null, mode: "hold" },
  async fetch(url, init = {}) {
    const u = new URL(url);
    this.requests.push({ path: u.pathname, method: init.method ?? "GET", headers: init.headers ?? {}, body: init.body });
    if (u.pathname === "/progress") return new Response(null, { status: 200 });
    if (u.pathname === "/cancel") {
      this.cancelState.polls += 1;
      if (init.signal?.aborted) throw new Error("aborted");
      if (this.cancelState.mode === "hold") {
        // First poll lapses (204 → re-arm); the second waits for the test to fire cancellation.
        if (this.cancelState.polls === 1) {
          await new Promise((r) => setTimeout(r, 0));
          return new Response(null, { status: 204 });
        }
        await new Promise((resolve) => (this.cancelState.release = resolve));
        return Response.json({ cancelled: true }, { status: 200 });
      }
      // `idle` mode: keep answering 204 until the adapter aborts the poll.
      await new Promise((resolve, reject) => {
        const t = setTimeout(resolve, 1);
        init.signal?.addEventListener("abort", () => {
          clearTimeout(t);
          reject(new Error("aborted"));
        });
      });
      return new Response(null, { status: 204 });
    }
    return new Response("nope", { status: 404 });
  },
};

const Adapter = wrapPluginFromBinding();
const env = {
  PLUGIN: new Plugin({}, {}),
  PLUGIN_STOREFRONT: new Storefront({}, {}),
  PLUGIN_STORAGE: new Storage({}, {}),
  PLUGIN_DATABASE_ADAPTER: new DatabaseAdapter({}, {}),
  PLUGIN_CLI: new Cli({}, {}),
  GRANTED: granted,
  BRIDGE_TOKEN: "t",
};
const adapter = new Adapter({}, env);

const contextJson = JSON.stringify(
  toBridgeJson({
    invocation: { id: "inv-1", accountId: "acct" },
    config: jsonPayload({ greeting: "hi", who: "acct" }),
    secrets: jsonPayload({ token: "s3cr3t" }),
  }),
);

async function invoke(iface, method, paramsCodec, params, { caps = null, target = null, ctx = contextJson } = {}) {
  const body = W.encodeMessage(paramsCodec, params ?? {}, caps ? capTableFor(caps) : undefined);
  return adapter.invoke(iface, method, ctx, caps ? JSON.stringify(caps) : null, target, body);
}

function capTableFor(descriptors) {
  let next = 0;
  return {
    exportCap() {
      const index = next++;
      assert.ok(index < descriptors.length, "test exported more caps than descriptors");
      return index;
    },
    importCap() {
      throw new Error("request encode never imports");
    },
  };
}

function decodeReply(reply, resultsCodec, caps) {
  assert.ok(reply.body instanceof Uint8Array, `reply carries bytes: ${JSON.stringify(reply)}`);
  assert.ok(Array.isArray(reply.caps));
  const table = {
    exportCap() {
      throw new Error("reply decode never exports");
    },
    importCap(index) {
      return index === null ? null : (caps ?? reply.caps)[index];
    },
  };
  return W.decodeMessage(resultsCodec, reply.body, table).result;
}

// --- ContentSource --------------------------------------------------------

{
  const reply = await invoke("ContentSource", "searchCatalog", W.ContentSourceSearchCatalogParamsCodec, {
    params: { query: "dune", region: "us", limit: 5, page: 1 },
  });
  assert.deepEqual(reply.caps, []);
  const result = decodeReply(reply, W.ContentSourceSearchCatalogResultsCodec);
  assert.equal(result.kind, "ok");
  assert.equal(result.value.hits.length, 2);
  assert.equal(result.value.hits[0].productId, "p1");
  assert.equal(result.value.hits[0].title, "hit for dune");
  assert.equal(result.value.hits[0].authors, "A");
  assert.deepEqual(calls.storefront, [["searchCatalog", "dune"]]);
}

{
  const missing = decodeReply(
    await invoke("ContentSource", "purchaseHint", W.ContentSourcePurchaseHintParamsCodec, {
      params: { productId: "unknown" },
    }),
    W.ContentSourcePurchaseHintResultsCodec,
  );
  assert.equal(missing.kind, "ok");
  assert.equal(missing.value.found, false);
  assert.equal(missing.value.hint.productId, "", "null hint encodes as the default struct");
  const found = decodeReply(
    await invoke("ContentSource", "purchaseHint", W.ContentSourcePurchaseHintParamsCodec, {
      params: { productId: "known" },
    }),
    W.ContentSourcePurchaseHintResultsCodec,
  );
  assert.equal(found.value.found, true);
  assert.equal(found.value.hint.productId, "known");
  const detail = decodeReply(
    await invoke("ContentSource", "catalogDetail", W.ContentSourceCatalogDetailParamsCodec, {
      params: { productId: "known" },
    }),
    W.ContentSourceCatalogDetailResultsCodec,
  );
  assert.equal(detail.value.found, true);
  assert.equal(detail.value.hit.title, "Known");
}

{
  const result = decodeReply(
    await invoke("ContentSource", "diagnose", W.ContentSourceDiagnoseParamsCodec, {}),
    W.ContentSourceDiagnoseResultsCodec,
  );
  assert.deepEqual(result, { kind: "ok", value: { lines: ["greeting=hi", "secret=s3cr3t", "inv=inv-1"] } });
  const accounts = decodeReply(
    await invoke("ContentSource", "listAccounts", W.ContentSourceListAccountsParamsCodec, {}),
    W.ContentSourceListAccountsResultsCodec,
  );
  assert.equal(accounts.value.accounts[0].accountId, "a1");
  const health = decodeReply(
    await invoke("ContentSource", "health", W.ContentSourceHealthParamsCodec, {}),
    W.ContentSourceHealthResultsCodec,
  );
  assert.deepEqual(health, { kind: "ok", value: { ok: true, detail: "" } });
}

// author throw → `err` inside the reply, never a transport failure
{
  const reply = await invoke("ContentSource", "login", W.ContentSourceLoginParamsCodec, {
    params: { accountId: "a", username: "u", password: "p" },
  });
  assert.equal(reply.error, undefined, "author failures are not transport failures");
  const result = decodeReply(reply, W.ContentSourceLoginResultsCodec);
  assert.deepEqual(result, { kind: "err", value: { code: "unsupported", message: "login not implemented" } });
}

// --- Destination ----------------------------------------------------------

{
  const found = decodeReply(
    await invoke("Destination", "head", W.DestinationHeadParamsCodec, { key: "present" }),
    W.DestinationHeadResultsCodec,
  );
  assert.equal(found.kind, "ok");
  assert.equal(found.value.found, true);
  assert.equal(found.value.meta.key, "present");
  assert.equal(found.value.meta.size, 3);
  assert.equal(found.value.meta.contentType, "text/plain");
  assert.equal(found.value.meta.etag, "");
  assert.equal(found.value.meta.sha256.byteLength, 0);
  const missing = decodeReply(
    await invoke("Destination", "head", W.DestinationHeadParamsCodec, { key: "absent" }),
    W.DestinationHeadResultsCodec,
  );
  assert.equal(missing.value.found, false);
  assert.equal(missing.value.meta.key, "");
}

{
  const page = decodeReply(
    await invoke("Destination", "list", W.DestinationListParamsCodec, { options: { prefix: "", cursor: "", limit: 0 } }),
    W.DestinationListResultsCodec,
  );
  assert.deepEqual(calls.list, { prefix: undefined, cursor: undefined, limit: undefined }, "zero values → omitted");
  assert.deepEqual(page, { kind: "ok", value: { objects: [{ key: "a/1", size: 3 }], nextCursor: "next" } });
  await invoke("Destination", "list", W.DestinationListParamsCodec, { options: { prefix: "a/", cursor: "c", limit: 5 } });
  assert.deepEqual(calls.list, { prefix: "a/", cursor: "c", limit: 5 });
}

{
  const copy = decodeReply(
    await invoke("Destination", "copy", W.DestinationCopyParamsCodec, { from: "ab", to: "cde" }),
    W.DestinationCopyResultsCodec,
  );
  assert.deepEqual(copy, { kind: "ok", value: { bytesCopied: 5 } });
  const del = decodeReply(
    await invoke("Destination", "delete", W.DestinationDeleteParamsCodec, { key: "x" }),
    W.DestinationDeleteResultsCodec,
  );
  assert.deepEqual(del, { kind: "ok" });
  const commit = decodeReply(
    await invoke("Destination", "commit", W.DestinationCommitParamsCodec, { key: "k", commitToken: "tok" }),
    W.DestinationCommitResultsCodec,
  );
  assert.equal(commit.value.key, "k");
  assert.equal(commit.value.bytesWritten, 7);
  assert.equal(commit.value.etag, "tok");
  const abort = decodeReply(
    await invoke("Destination", "abortStage", W.DestinationAbortStageParamsCodec, { key: "k", commitToken: "tok" }),
    W.DestinationAbortStageResultsCodec,
  );
  assert.deepEqual(abort, { kind: "err", value: { code: "not_found", message: "no such stage" } });
}

// --- EventConsumer: one author call per batch -----------------------------

{
  const event = (id) => ({
    eventId: id,
    eventType: "book_acquired",
    schemaVersion: 2,
    occurredAtUnixMs: 1_700_000_000_000,
    deliveryAttempt: 1,
    accountId: "acct",
    source: "echo",
    correlationId: "",
    causationId: "",
    deduplicationKey: id,
    payload: enc.encode("{}"),
    invocationSequence: 0,
    resumePending: false,
    checkpointJson: "",
    checkpointSchemaVersion: 0,
  });
  const result = decodeReply(
    await invoke("EventConsumer", "event", W.EventConsumerEventParamsCodec, {
      batch: { events: [event("evt-1"), event("evt-2")] },
    }),
    W.EventConsumerEventResultsCodec,
  );
  assert.equal(calls.event, 1, "the whole batch is one author call");
  assert.deepEqual(result, {
    kind: "ok",
    value: [
      { kind: "ack", value: { dummy: undefined } },
      { kind: "retry", value: { retryAtUnixMs: 0, reason: "again" } },
    ],
  });
}

// --- JobRunner: four capability descriptors -------------------------------

const jobInvocation = {
  payloadSchemaVersion: 1,
  invocationId: "job-1",
  commandType: "echo.copy",
  payloadJson: '{"n":1}',
  idempotencyKey: "idem",
  attempt: 1,
  correlationId: "",
  causationId: "",
  deadlineUnixMs: 0,
  checkpointJson: "",
  checkpointSchemaVersion: 0,
  invocationSequence: 0,
  stepId: "",
};
const jobCaps = [
  { kind: "source", token: "tok-in" },
  { kind: "destination", token: "tok-out" },
  { kind: "progress", token: "tok-progress" },
  { kind: "cancellation", token: "tok-cancel" },
];
const jobParams = (commandType) => ({
  controller: { invocation: { ...jobInvocation, commandType }, input: "in", output: "out", progress: "p", cancel: "c" },
});

{
  granted.requests.length = 0;
  granted.cancelState = { polls: 0, release: null, mode: "idle" };
  const result = decodeReply(
    await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("echo.copy"), { caps: jobCaps }),
    W.JobRunnerJobResultsCodec,
  );
  assert.deepEqual(result, {
    kind: "ok",
    value: { kind: "completed", value: { message: "ran echo.copy for acct", bytesCopied: 9 } },
  });
  const progress = granted.requests.find((r) => r.path === "/progress");
  assert.ok(progress, "GrantedProgress posted to the granted channel");
  assert.equal(progress.method, "POST");
  assert.equal(progress.headers.Authorization, "Bearer tok-progress");
  assert.deepEqual(JSON.parse(progress.body), { percent: 50, message: "half" });
  const cancelPoll = granted.requests.find((r) => r.path === "/cancel");
  assert.ok(cancelPoll, "the cancel watch long-polls the granted channel");
  assert.equal(cancelPoll.headers.Authorization, "Bearer tok-cancel");
  const pollsAtReturn = granted.cancelState.polls;
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(granted.cancelState.polls, pollsAtReturn, "polling stops once the job call returns");
}

{
  granted.requests.length = 0;
  granted.cancelState = { polls: 0, release: null, mode: "hold" };
  jobHooks.onProgressed = () => {
    // Fire host cancellation once the second poll is armed.
    const fire = () => {
      if (granted.cancelState.release) granted.cancelState.release();
      else setTimeout(fire, 0);
    };
    fire();
  };
  const result = decodeReply(
    await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("cancel-me"), { caps: jobCaps }),
    W.JobRunnerJobResultsCodec,
  );
  jobHooks.onProgressed = undefined;
  assert.deepEqual(result, { kind: "ok", value: { kind: "cancelled", value: { message: "stopped" } } });
  assert.equal(granted.cancelState.polls, 2, "204 re-arms the poll; 200 resolves it");
}

// bad capability table → 400
{
  const reply = await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("x"), {
    caps: [{ kind: "source", token: "a" }, { kind: "bogus" }, { kind: "progress" }, { kind: "cancellation" }],
  });
  assert.equal(reply.status, 400);
  assert.equal(reply.error.code, "invalid_params");
  const short = await adapter.invoke(
    "JobRunner",
    "job",
    contextJson,
    JSON.stringify([{ kind: "source" }]),
    null,
    W.encodeMessage(W.JobRunnerJobParamsCodec, jobParams("x"), capTableFor(jobCaps)),
  );
  assert.equal(short.status, 400, "capability index without a descriptor");
}

// --- Database.openSession → AdapterDatabaseSession.* by target -------------

{
  const opened = await invoke("Database", "openSession", W.DatabaseOpenSessionParamsCodec, {});
  assert.equal(opened.caps.length, 1);
  assert.equal(opened.caps[0].kind, "adapterSession");
  assert.match(opened.caps[0].id, /^[0-9a-f-]{32,36}$/);
  const result = decodeReply(opened, W.DatabaseOpenSessionResultsCodec);
  assert.equal(result.kind, "ok");
  assert.deepEqual(result.value, opened.caps[0], "the ok pointer is capability 0");
  const id = opened.caps[0].id;

  const caps = decodeReply(
    await invoke("AdapterDatabaseSession", "capabilities", W.AdapterDatabaseSessionCapabilitiesParamsCodec, {}, {
      target: id,
    }),
    W.AdapterDatabaseSessionCapabilitiesResultsCodec,
  );
  assert.equal(caps.kind, "ok");
  assert.equal(caps.value.sqlContractVersion, 1);
  assert.equal(caps.value.atomicBatch, true);
  assert.equal(caps.value.schemaMigrations, false);

  const relations = decodeReply(
    await invoke("AdapterDatabaseSession", "listUserRelations", W.AdapterDatabaseSessionListUserRelationsParamsCodec, {}, {
      target: id,
    }),
    W.AdapterDatabaseSessionListUserRelationsResultsCodec,
  );
  assert.deepEqual(relations, { kind: "ok", value: ["books", "authors"] });
  const dropped = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "dropUserRelations",
      W.AdapterDatabaseSessionDropUserRelationsParamsCodec,
      { names: ["books"] },
      { target: id },
    ),
    W.AdapterDatabaseSessionDropUserRelationsResultsCodec,
  );
  assert.deepEqual(dropped, { kind: "ok" });
  assert.deepEqual(calls.dropped, ["books"]);
  const unsupported = decodeReply(
    await invoke("AdapterDatabaseSession", "bootstrap", W.AdapterDatabaseSessionBootstrapParamsCodec, {}, { target: id }),
    W.AdapterDatabaseSessionBootstrapResultsCodec,
  );
  assert.deepEqual(unsupported, { kind: "err", value: { code: "unsupported", message: "bootstrap not implemented" } });

  const noTarget = await invoke("AdapterDatabaseSession", "close", W.AdapterDatabaseSessionCloseParamsCodec, {});
  assert.equal(noTarget.status, 400, "session calls need X-Bookclerk-Target");

  const closed = decodeReply(
    await invoke("AdapterDatabaseSession", "close", W.AdapterDatabaseSessionCloseParamsCodec, {}, { target: id }),
    W.AdapterDatabaseSessionCloseResultsCodec,
  );
  assert.deepEqual(closed, { kind: "ok" });
  const gone = decodeReply(
    await invoke("AdapterDatabaseSession", "capabilities", W.AdapterDatabaseSessionCapabilitiesParamsCodec, {}, {
      target: id,
    }),
    W.AdapterDatabaseSessionCapabilitiesResultsCodec,
  );
  assert.equal(gone.kind, "err");
  assert.equal(gone.value.code, "invalid_params", "closed sessions are released");
}

// --- PluginWorker.databaseMigrations (no context header) ------------------

{
  const result = decodeReply(
    await invoke("PluginWorker", "databaseMigrations", W.PluginWorkerDatabaseMigrationsParamsCodec, { binding: "DB" }, {
      ctx: null,
    }),
    W.PluginWorkerDatabaseMigrationsResultsCodec,
  );
  assert.deepEqual(result, {
    kind: "ok",
    value: {
      migrations: [
        {
          id: "DB-001",
          operations: [
            { kind: "schema", value: "CREATE TABLE seen (id TEXT)" },
            { kind: "data", value: "INSERT INTO seen VALUES ('x')" },
          ],
        },
      ],
    },
  });
}

// --- PluginCli ------------------------------------------------------------

{
  const schema = decodeReply(
    await invoke("PluginCli", "describe", W.PluginCliDescribeParamsCodec, {}),
    W.PluginCliDescribeResultsCodec,
  );
  assert.equal(schema.kind, "ok");
  assert.equal(schema.value.commands[0].name, "ping");
}

// --- Transport failures ---------------------------------------------------

{
  const unknown = await invoke("ContentSource", "bogus", W.ContentSourceHealthParamsCodec, {});
  assert.equal(unknown.status, 400);
  assert.equal(unknown.error.code, "invalid_params");
  const unknownIface = await invoke("Nope", "health", W.ContentSourceHealthParamsCodec, {});
  assert.equal(unknownIface.status, 400);
  const proto = await invoke("ContentSource", "constructor", W.ContentSourceHealthParamsCodec, {});
  assert.equal(proto.status, 400, "prototype keys are not methods");
  const streamRoute = await invoke("Destination", "get", W.DestinationHeadParamsCodec, { key: "k" });
  assert.equal(streamRoute.status, 400, "get/put stay stream routes");

  const missingBinding = await invoke("RemoteLibrary", "health", W.RemoteLibraryHealthParamsCodec, {});
  assert.equal(missingBinding.status, 404);
  assert.equal(missingBinding.error.code, "unsupported");

  const malformed = await adapter.invoke("ContentSource", "health", contextJson, null, null, new Uint8Array([1, 2, 3]));
  assert.equal(malformed.status, 400);
  const badContext = await adapter.invoke(
    "ContentSource",
    "health",
    "{not json",
    null,
    null,
    W.encodeMessage(W.ContentSourceHealthParamsCodec, {}),
  );
  assert.equal(badContext.status, 400);

  const noPlugin = new Adapter({}, { ...env, PLUGIN: undefined });
  const unavailable = await noPlugin.invoke(
    "EventConsumer",
    "event",
    contextJson,
    null,
    null,
    W.encodeMessage(W.EventConsumerEventParamsCodec, { batch: { events: [] } }),
  );
  assert.equal(unavailable.status, 500);
  assert.equal(unavailable.error.code, "unavailable");

  // The dispatcher itself throws `InvokeError`; the adapter turns it into the plain failure value.
  await assert.rejects(
    () =>
      dispatchInvoke("ContentSource", "bogus", undefined, [], null, new Uint8Array(), {
        author: () => env.PLUGIN,
        named: () => undefined,
        bindContext: (ctx) => ctx ?? {},
        importCap: () => undefined,
      }),
    (err) => err instanceof InvokeError && err.status === 400 && err.wireCode === "invalid_params",
  );
}

// --- Embed parity ---------------------------------------------------------

{
  const embed = await import(join(root, "../embed/bookclerk_plugin.js"));
  assert.equal(typeof embed.wrapPluginFromBinding, "function");
  assert.equal(typeof embed.InvokeError, "function");
  const EmbedAdapter = embed.wrapPluginFromBinding();
  const embedAdapter = new EmbedAdapter({}, { ...env, PLUGIN_STOREFRONT: new embed.StorefrontEntrypoint({}, {}) });
  const reply = await embedAdapter.invoke(
    "ContentSource",
    "health",
    contextJson,
    null,
    null,
    W.encodeMessage(W.ContentSourceHealthParamsCodec, {}),
  );
  assert.deepEqual(decodeReply(reply, W.ContentSourceHealthResultsCodec), {
    kind: "ok",
    value: { ok: true, detail: "" },
  });
}

console.log("adapter /invoke dispatch ok");
