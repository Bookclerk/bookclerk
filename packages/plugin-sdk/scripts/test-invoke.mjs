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
  MetadataEntrypoint,
  CoverEntrypoint,
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
const { decodeExecuteRequest, encodeDbCapabilitiesReply, encodeExecuteResultReply } = await import(
  join(dist, "db-execute.js")
);
const { limitExceededMessage } = await import(join(dist, "errors.js"));
const W = await import(join(dist, "generated-wire.js"));

const enc = new TextEncoder();

// --- Author isolate stand-ins --------------------------------------------

const calls = { event: 0, queue: 0, queueSends: [], storefront: [], scheduled: [] };

class Plugin extends BookclerkEntrypoint {
  async scheduled(controller) {
    calls.scheduled.push(controller);
    if (controller.cron === "0 0 * * *") throw new Error("nightly failed");
  }

  async queue(batch) {
    if (batch.queue === "db-probe") {
      calls.dbProbe = await probeGrantedDatabases(this.env);
      for (const msg of batch.messages) msg.ack();
      return;
    }
    calls.queue += 1;
    const sent = await this.env.NOTIFY.send({ seen: batch.queue }, { delaySeconds: 3 });
    calls.queueSends.push(sent);
    // `env.KV` round-trips through the adapter's kv grant.
    const kv = this.env.KV;
    calls.kvMissing = await kv.get("cursor");
    await kv.put("cursor", "page-7", { expirationTtl: 60, metadata: { page: 7 } });
    calls.kvText = await kv.get("cursor");
    calls.kvJson = await kv.getWithMetadata("cursor", "json").catch((err) => String(err));
    calls.kvBytes = new TextDecoder().decode(await kv.get("cursor", "arrayBuffer"));
    // `env.DB.limits()` on a host without the route is `unsupported`, not
    // `unavailable`: the author can tell "older host" from "try later".
    calls.dbLimits = await this.env.DB.limits().catch((err) => err);
    calls.kvList = await kv.list({ prefix: "cur" });
    await kv.delete("cursor");
    calls.kvAfterDelete = await kv.get("cursor");
    calls.kvTooBig = await kv.put("big", new Uint8Array(1_048_577)).catch((err) => err.code);
    for (const msg of batch.messages) {
      if (msg.id === "msg-2") msg.retry({ delaySeconds: 7, reason: "again" });
      else msg.ack();
    }
  }
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
        operations: [
          schemaMigrationOp("CREATE TABLE seen (id TEXT)"),
          dataMigrationOp("INSERT INTO seen VALUES ('x')"),
        ],
      },
    ];
  }
}
const jobHooks = {};

class Storefront extends StorefrontEntrypoint {
  async searchCatalog(params) {
    calls.storefront.push(["searchCatalog", params.query]);
    return [
      {
        productId: "p1",
        title: `hit for ${params.query}`,
        credits: [{ name: "A", role: "author" }],
      },
      { productId: "p2", title: "second" },
    ];
  }
  async purchaseHint(params) {
    return params.productId === "known" ? { productId: "known", title: "Known" } : null;
  }
  async catalogDetail(params) {
    return params.productId === "known" ? { productId: "known", title: "Known" } : null;
  }
  async revokeAccount(params) {
    calls.storefront.push([
      "revokeAccount",
      params.accountId,
      Array.from(params.credentials ?? []),
    ]);
    if (params.accountId === "bad") throw new Error("store refused sign-out");
  }
  async diagnose() {
    return [
      `greeting=${this.env.CONFIG.greeting}`,
      `secret=${this.env.SECRETS.token}`,
      `inv=${this.invocation.id}`,
    ];
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
      // Past one scalar: the adapter clamps it before the host sees it.
      maxStreamedResultBytes: 64 * 1024 * 1024,
      maxStreamedResultRows: 250_000,
    };
  }
  async executeStreamed(request) {
    calls.streamed = request;
    switch (request.operationId) {
      case "frames-small":
        return new Uint8Array([1, 2, 3, 4]);
      case "frames-big":
        return new Uint8Array(262_145);
      default:
        // A look-alike that is not a real Uint8Array.
        return { byteLength: 4, length: 4 };
    }
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

class Metadata extends MetadataEntrypoint {
  async lookup(params) {
    const isbn = params.identifiers.find((id) => id.scheme === "isbn");
    if (!isbn) return [];
    return [
      {
        title: "Dune",
        mediaKind: "print",
        identifiers: [
          { scheme: "isbn", value: isbn.value },
          { scheme: "openLibraryWork", value: "OL893415W" },
        ],
        credits: [{ name: "Frank Herbert", role: "author" }],
        seriesEntries: [{ name: "Dune", position: "1" }],
      },
    ];
  }
  async search(params) {
    // The wire envelope is not a valid return value.
    if (params.title === "wrapped") return { records: [] };
    return [{ title: `${params.title} by ${params.author ?? "anyone"}` }];
  }
  async chapters(params) {
    if (params.identifiers[0]?.value !== "B002V1OF70") return { found: false, chapters: [] };
    return {
      found: true,
      accurate: true,
      brandIntroMs: 2000,
      chapters: [
        { title: "Book One", startMs: 0, depth: 0 },
        { title: "Chapter 1", startMs: 2000, lengthMs: 60000, depth: 1 },
      ],
    };
  }
}

class Covers extends CoverEntrypoint {
  async fetchCovers(params, sink) {
    // The wire envelope is not a valid return value.
    if (params.urls[0] === "https://wrapped.example/") return { images: [] };
    const images = [];
    for (const [i, url] of params.urls.entries()) {
      if (url.endsWith("/missing.jpg")) {
        images.push({ url, stored: false, error: "HTTP 404" });
        continue;
      }
      const bytes = new TextEncoder().encode(`img:${i}`);
      await sink.put(String(i), new Blob([bytes]).stream(), {
        contentType: "image/jpeg",
        contentLength: bytes.length,
      });
      images.push({ url, stored: true, contentType: "image/jpeg" });
    }
    return images;
  }
}

// --- Granted database errors across the adapter → author RPC boundary -----

// The canonical `limit_exceeded` the fake channel answers with `413`.
const requestLimit = { limit: "maxRequestBytes", cap: 100, used: 200 };

// A session that streams, so reads try `/db/execute-streamed` first.
const streamingCaps = {
  sqlContractVersion: 1,
  atomicBatch: true,
  returning: true,
  affectedRows: true,
  schemaMigrations: true,
  cancellation: true,
  timing: true,
  maxBinds: 999,
  maxStatements: 100,
  maxResultRows: 10000,
  maxPayloadBytes: 1 << 20,
  maxResultBytes: 1 << 20,
  maxCellBytes: 1 << 20,
  maxRequestBytes: 1 << 20,
  maxAtomicResultBytes: 1 << 20,
  maxStreamedResultBytes: 64 << 20,
  maxStreamedResultRows: 250_000,
  pluginDatabases: true,
  maxFunctionArgs: 127,
  maxSchemaColumns: 2000,
  maxPatternBytes: 50000,
  maxLoweredStatementBytes: 0,
  consistentBackupRead: true,
};

/**
 * Runs the author's database reads against the channel answers the fake is
 * set to, collecting each outcome. `DB` has no `/db/limits` route (reads go
 * inline); `SDB` streams.
 *
 * @param env - The author's invocation env.
 * @returns Each read's `{ value }` or `{ err }`, by probe name.
 */
async function probeGrantedDatabases(env) {
  const out = {};
  const settle = (promise) =>
    promise.then(
      (value) => ({ value }),
      (err) => ({ err }),
    );
  granted.db = { execute: 413 };
  out.inline413 = await settle(env.DB.prepare("INSERT INTO t VALUES (1)").run());
  granted.db = { execute: 503 };
  out.inline503 = await settle(env.DB.prepare("SELECT 1").all());
  granted.db = { execute: "ok", streamed: 404 };
  out.streamed404 = await settle(env.SDB.prepare("SELECT 1").all());
  out.iterate404 = await settle(
    (async () => {
      const rows = [];
      for await (const row of env.SDB.prepare("SELECT 1").iterate()) rows.push(row);
      return rows;
    })(),
  );
  granted.db = { execute: "ok", streamed: 413 };
  out.streamed413 = await settle(env.SDB.prepare("SELECT 1").all());
  granted.db = { execute: "ok", streamed: "throw" };
  out.streamedThrow = await settle(env.SDB.prepare("SELECT 1").all());
  granted.db = undefined;
  return out;
}

/**
 * Stands in for Workers RPC between the adapter and the author isolate: an
 * error thrown by a stub method arrives as a plain `Error` carrying only its
 * message, as workerd delivers it, so a `PluginError` loses its code and
 * `details` unless the transport reports it inside the reply.
 *
 * @param target - The transport the adapter handed out.
 * @returns The transport as the author isolate sees it.
 */
function rpcStub(target) {
  return new Proxy(target, {
    get(obj, prop) {
      const value = Reflect.get(obj, prop, obj);
      if (typeof value !== "function") return value;
      const method = async (...args) => {
        try {
          return await value.apply(obj, args);
        } catch (err) {
          throw new Error(err instanceof Error ? err.message : String(err));
        }
      };
      // A detached stub property is itself a stub: `m.call(...)` asks the
      // remote for a method named `call`, as workerd does.
      return new Proxy(method, {
        get(fn, key) {
          if (key === "call" || key === "apply" || key === "bind") {
            return () => {
              throw new Error(`The RPC receiver does not implement the method "${key}".`);
            };
          }
          return Reflect.get(fn, key);
        },
      });
    },
  });
}

/**
 * The author entrypoint as the adapter reaches it: transports cross RPC.
 *
 * @param author - The author entrypoint instance.
 * @returns The entrypoint whose `bookclerk*` methods see RPC transports.
 */
function acrossRpc(author) {
  const crossing = (arg) =>
    arg && typeof arg === "object" && Array.isArray(arg.databases)
      ? {
          ...arg,
          databases: arg.databases.map((entry) =>
            entry.transport ? { ...entry, transport: rpcStub(entry.transport) } : entry,
          ),
        }
      : arg;
  return new Proxy(author, {
    get(obj, prop) {
      const value = Reflect.get(obj, prop, obj);
      if (
        typeof value !== "function" ||
        typeof prop !== "string" ||
        !prop.startsWith("bookclerk")
      ) {
        return value;
      }
      return (...args) => value.apply(obj, args.map(crossing));
    },
  });
}

// --- Granted channel fake -------------------------------------------------

const granted = {
  requests: [],
  cancelState: { polls: 0, release: null, mode: "hold" },
  async fetch(url, init = {}) {
    const u = new URL(url);
    this.requests.push({
      path: u.pathname,
      method: init.method ?? "GET",
      headers: init.headers ?? {},
      body: init.body,
    });
    if (u.pathname === "/progress") return new Response(null, { status: 200 });
    if (u.pathname === "/put") {
      const bytes = new Uint8Array(await new Response(init.body).arrayBuffer());
      this.puts = [...(this.puts ?? []), { key: u.searchParams.get("key"), bytes }];
      return Response.json({ key: u.searchParams.get("key"), bytesWritten: bytes.length });
    }
    if (u.pathname.startsWith("/kv/")) {
      const body = JSON.parse(init.body);
      const store = (this.kv ??= new Map());
      switch (u.pathname) {
        case "/kv/get": {
          const row = store.get(body.key);
          return Response.json(
            row
              ? { found: true, value: row.value, metadata: row.metadata, expirationUnixMs: 0 }
              : { found: false },
          );
        }
        case "/kv/put":
          store.set(body.key, {
            value: body.value,
            metadata: body.options.metadata,
            options: body.options,
          });
          return Response.json({ expirationUnixMs: 0 });
        case "/kv/delete":
          return Response.json({ existed: store.delete(body.key) });
        case "/kv/list":
          return Response.json({
            keys: [...store.keys()]
              .filter((k) => k.startsWith(body.options.prefix))
              .map((key) => ({ key, metadata: store.get(key).metadata, expirationUnixMs: 61_000 })),
            listComplete: true,
            cursor: "",
          });
        default:
          return new Response("nope", { status: 404 });
      }
    }
    if (u.pathname === "/db/limits") {
      if (init.headers?.Authorization === "Bearer stok") {
        return new Response(encodeDbCapabilitiesReply({ ok: streamingCaps }), { status: 200 });
      }
      return new Response("not found", { status: 404 });
    }
    if (u.pathname === "/db/execute") {
      const mode = this.db?.execute;
      if (mode === 413) return new Response(limitExceededMessage(requestLimit), { status: 413 });
      if (mode === "ok") {
        const request = decodeExecuteRequest(new Uint8Array(init.body));
        const reply = {
          operationId: request.operationId,
          statements: request.statements.map(() => ({ rows: [], columns: [], rowsAffected: 0 })),
          timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "" },
        };
        return new Response(encodeExecuteResultReply({ ok: reply }), { status: 200 });
      }
      return new Response("database adapter is restarting", { status: 503 });
    }
    if (u.pathname === "/db/execute-streamed") {
      const mode = this.db?.streamed;
      if (mode === "throw") throw new TypeError("granted channel reset");
      if (mode === 413) return new Response(limitExceededMessage(requestLimit), { status: 413 });
      return new Response("not found", { status: 404 });
    }
    if (u.pathname === "/queues/send") {
      const sent = JSON.parse(init.body);
      return Response.json(
        { messageIds: sent.messages.map((_, i) => `q-${i + 1}`) },
        { status: 200 },
      );
    }
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
  PLUGIN: acrossRpc(new Plugin({}, {})),
  PLUGIN_STOREFRONT: new Storefront({}, {}),
  PLUGIN_STORAGE: new Storage({}, {}),
  PLUGIN_DATABASE_ADAPTER: new DatabaseAdapter({}, {}),
  PLUGIN_CLI: new Cli({}, {}),
  PLUGIN_METADATA: new Metadata({}, {}),
  PLUGIN_COVERS: new Covers({}, {}),
  GRANTED: granted,
  BRIDGE_TOKEN: "t",
};
const adapter = new Adapter({}, env);

const contextJson = JSON.stringify(
  toBridgeJson({
    invocation: { id: "inv-1", accountId: "acct" },
    config: jsonPayload({ greeting: "hi", who: "acct" }),
    secrets: jsonPayload({ token: "s3cr3t" }),
    queues: { NOTIFY: "qtok" },
    kv: { KV: "ktok" },
    databases: { DB: "dtok" },
  }),
);

async function invoke(
  iface,
  method,
  paramsCodec,
  params,
  { caps = null, target = null, ctx = contextJson } = {},
) {
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
  const reply = await invoke(
    "ContentSource",
    "searchCatalog",
    W.ContentSourceSearchCatalogParamsCodec,
    {
      params: { query: "dune", region: "us", limit: 5, page: 1 },
    },
  );
  assert.deepEqual(reply.caps, []);
  const result = decodeReply(reply, W.ContentSourceSearchCatalogResultsCodec);
  assert.equal(result.kind, "ok");
  assert.equal(result.value.hits.length, 2);
  assert.equal(result.value.hits[0].productId, "p1");
  assert.equal(result.value.hits[0].title, "hit for dune");
  assert.equal(result.value.hits[0].credits[0].name, "A");
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
  const revoked = decodeReply(
    await invoke("ContentSource", "revokeAccount", W.ContentSourceRevokeAccountParamsCodec, {
      params: { accountId: "a1", credentials: new Uint8Array([1, 2, 3]) },
    }),
    W.ContentSourceRevokeAccountResultsCodec,
  );
  assert.equal(revoked.kind, "ok");
  assert.deepEqual(calls.storefront.at(-1), ["revokeAccount", "a1", [1, 2, 3]]);
  const refused = decodeReply(
    await invoke("ContentSource", "revokeAccount", W.ContentSourceRevokeAccountParamsCodec, {
      params: { accountId: "bad" },
    }),
    W.ContentSourceRevokeAccountResultsCodec,
  );
  assert.equal(refused.kind, "err");
}

{
  const result = decodeReply(
    await invoke("ContentSource", "diagnose", W.ContentSourceDiagnoseParamsCodec, {}),
    W.ContentSourceDiagnoseResultsCodec,
  );
  assert.deepEqual(result, {
    kind: "ok",
    value: { lines: ["greeting=hi", "secret=s3cr3t", "inv=inv-1"] },
  });
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
  assert.deepEqual(result, {
    kind: "err",
    value: { code: "unsupported", message: "login not implemented", details: "" },
  });
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
    await invoke("Destination", "list", W.DestinationListParamsCodec, {
      options: { prefix: "", cursor: "", limit: 0 },
    }),
    W.DestinationListResultsCodec,
  );
  assert.deepEqual(
    calls.list,
    { prefix: undefined, cursor: undefined, limit: undefined },
    "zero values → omitted",
  );
  assert.deepEqual(page, {
    kind: "ok",
    value: { objects: [{ key: "a/1", size: 3 }], nextCursor: "next" },
  });
  await invoke("Destination", "list", W.DestinationListParamsCodec, {
    options: { prefix: "a/", cursor: "c", limit: 5 },
  });
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
    await invoke("Destination", "commit", W.DestinationCommitParamsCodec, {
      key: "k",
      commitToken: "tok",
    }),
    W.DestinationCommitResultsCodec,
  );
  assert.equal(commit.value.key, "k");
  assert.equal(commit.value.bytesWritten, 7);
  assert.equal(commit.value.etag, "tok");
  const abort = decodeReply(
    await invoke("Destination", "abortStage", W.DestinationAbortStageParamsCodec, {
      key: "k",
      commitToken: "tok",
    }),
    W.DestinationAbortStageResultsCodec,
  );
  assert.deepEqual(abort, {
    kind: "err",
    value: { code: "not_found", message: "no such stage", details: "" },
  });
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

// --- Granted database: typed errors survive the adapter → author RPC hop ----
//
// `env.PLUGIN` is wrapped by `acrossRpc`, so every transport method the author
// calls behaves as over Workers RPC: a thrown error loses its prototype. The
// author still sees typed `PluginError`s because the binding reads through
// the transport's `*Bytes` methods, which report failures inside the reply.

{
  const ctx = JSON.stringify(
    toBridgeJson({
      invocation: { id: "inv-db", accountId: "acct" },
      databases: { DB: "dtok", SDB: "stok" },
    }),
  );
  const result = decodeReply(
    await invoke(
      "QueueConsumer",
      "queue",
      W.QueueConsumerQueueParamsCodec,
      { batch: { queue: "db-probe", messages: [] } },
      { ctx },
    ),
    W.QueueConsumerQueueResultsCodec,
  );
  assert.equal(result.kind, "ok");
  const probe = calls.dbProbe;

  // `413`: `limit_exceeded` with its parsed `limit`, not message matching.
  const tooBig = probe.inline413.err;
  assert.ok(tooBig instanceof PluginError, String(tooBig));
  assert.equal(tooBig.code, "limit_exceeded");
  assert.deepEqual(tooBig.limit, requestLimit);
  assert.deepEqual(tooBig.details, requestLimit);

  // Any other status is `unavailable`, still typed.
  const down = probe.inline503.err;
  assert.ok(down instanceof PluginError, String(down));
  assert.equal(down.code, "unavailable");
  assert.match(down.message, /database grant: 503 database adapter is restarting/);

  // A host without the streamed route answers `unsupported`, so `all()` and
  // `iterate()` fall back to the inline read instead of failing.
  assert.equal(probe.streamed404.err, undefined, String(probe.streamed404.err));
  assert.equal(probe.streamed404.value.success, true);
  assert.equal(probe.iterate404.err, undefined, String(probe.iterate404.err));
  assert.deepEqual(probe.iterate404.value, []);

  // A streamed `413` keeps its `limit` too.
  const streamedTooBig = probe.streamed413.err;
  assert.ok(streamedTooBig instanceof PluginError, String(streamedTooBig));
  assert.equal(streamedTooBig.code, "limit_exceeded");
  assert.deepEqual(streamedTooBig.limit, requestLimit);

  // A channel fetch that throws is `unavailable` rather than an untyped error.
  const reset = probe.streamedThrow.err;
  assert.ok(reset instanceof PluginError, String(reset));
  assert.equal(reset.code, "unavailable");
  assert.match(reset.message, /granted channel reset/);
}

// --- ScheduledHandler: one author call per fire; a throw is a typed error ----

{
  const fire = (cron) => ({
    cron,
    scheduledTime: 1_800_000,
    fireId: "plugin_scheduled-1",
    attempt: 2,
    deadlineUnixMs: 1_860_000,
  });
  const ok = decodeReply(
    await invoke("ScheduledHandler", "scheduled", W.ScheduledHandlerScheduledParamsCodec, {
      controller: fire("*/30 * * * *"),
    }),
    W.ScheduledHandlerScheduledResultsCodec,
  );
  assert.equal(ok.kind, "ok");
  assert.deepEqual(calls.scheduled[0], fire("*/30 * * * *"));
  assert.ok(Object.isFrozen(calls.scheduled[0]), "the controller is read-only");
  const failed = decodeReply(
    await invoke("ScheduledHandler", "scheduled", W.ScheduledHandlerScheduledParamsCodec, {
      controller: fire("0 0 * * *"),
    }),
    W.ScheduledHandlerScheduledResultsCodec,
  );
  assert.equal(failed.kind, "err");
  assert.match(String(failed.value.message), /nightly failed/);
}

// --- QueueConsumer: one author call per batch; env.<QUEUE> sends via GRANTED --

{
  const message = (id) => ({
    id,
    timestampUnixMs: 1_700_000_000_000,
    body: enc.encode('{"n":1}'),
    contentType: "json",
    attempts: 1,
    source: "echo",
  });
  const before = granted.requests.length;
  const result = decodeReply(
    await invoke("QueueConsumer", "queue", W.QueueConsumerQueueParamsCodec, {
      batch: { queue: "jobs", messages: [message("msg-1"), message("msg-2")] },
    }),
    W.QueueConsumerQueueResultsCodec,
  );
  assert.equal(calls.queue, 1, "the whole batch is one author call");
  assert.deepEqual(result, {
    kind: "ok",
    value: {
      results: [{ kind: "ack" }, { kind: "retry", value: { delaySeconds: 7, reason: "again" } }],
    },
  });
  assert.deepEqual(
    calls.queueSends,
    [{ messageIds: ["q-1"] }],
    "send returns the host-assigned ids",
  );
  const send = granted.requests.slice(before).find((r) => r.path === "/queues/send");
  assert.ok(send, "env.NOTIFY.send went through the granted channel");
  assert.equal(
    send.headers.Authorization,
    "Bearer qtok",
    "the queue grant token never reaches the author",
  );
  const body = JSON.parse(send.body);
  assert.equal(body.delaySeconds, 0);
  assert.equal(body.messages.length, 1);
  assert.equal(body.messages[0].contentType, "json");
  assert.equal(body.messages[0].delaySeconds, 3);
  assert.equal(Buffer.from(body.messages[0].body, "base64").toString(), '{"seen":"jobs"}');

  const limits = granted.requests.slice(before).find((r) => r.path === "/db/limits");
  assert.ok(limits, "env.DB.limits went through the granted channel");
  assert.equal(limits.headers.Authorization, "Bearer dtok");
  assert.ok(calls.dbLimits instanceof PluginError, String(calls.dbLimits));
  assert.equal(calls.dbLimits.code, "unsupported", calls.dbLimits.message);
  assert.match(calls.dbLimits.message, /GuestDatabase\.limits is not served by this host/);

  // env.KV: Workers KVNamespace shape over `/kv/*` with the kv-only token.
  assert.equal(calls.kvMissing, null, "absent key reads as null");
  assert.equal(calls.kvText, "page-7");
  assert.match(
    String(calls.kvJson),
    /JSON|Unexpected token/,
    "text that is not JSON fails the json decode",
  );
  assert.equal(calls.kvBytes, "page-7");
  assert.deepEqual(calls.kvList, {
    keys: [{ name: "cursor", expiration: 61, metadata: { page: 7 } }],
    list_complete: true,
  });
  assert.equal(calls.kvAfterDelete, null);
  assert.equal(calls.kvTooBig, "payload_too_large", "oversize values never leave the isolate");
  const kvCalls = granted.requests.slice(before).filter((r) => r.path.startsWith("/kv/"));
  assert.ok(kvCalls.length >= 6, "every KV op went through the granted channel");
  assert.ok(
    kvCalls.every((r) => r.headers.Authorization === "Bearer ktok"),
    "the kv grant token never reaches the author",
  );
  const put = JSON.parse(kvCalls.find((r) => r.path === "/kv/put").body);
  assert.equal(Buffer.from(put.value, "base64").toString(), "page-7");
  assert.deepEqual(put.options, {
    expirationUnixMs: 0,
    expirationTtlSeconds: 60,
    metadata: '{"page":7}',
  });
  assert.ok(!kvCalls.some((r) => r.path === "/kv/put" && JSON.parse(r.body).key === "big"));
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
  controller: {
    invocation: { ...jobInvocation, commandType },
    input: "in",
    output: "out",
    progress: "p",
    cancel: "c",
  },
});

{
  granted.requests.length = 0;
  granted.cancelState = { polls: 0, release: null, mode: "idle" };
  const result = decodeReply(
    await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("echo.copy"), {
      caps: jobCaps,
    }),
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
    await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("cancel-me"), {
      caps: jobCaps,
    }),
    W.JobRunnerJobResultsCodec,
  );
  jobHooks.onProgressed = undefined;
  assert.deepEqual(result, {
    kind: "ok",
    value: { kind: "cancelled", value: { message: "stopped" } },
  });
  assert.equal(granted.cancelState.polls, 2, "204 re-arms the poll; 200 resolves it");
}

// bad capability table → 400
{
  const reply = await invoke("JobRunner", "job", W.JobRunnerJobParamsCodec, jobParams("x"), {
    caps: [
      { kind: "source", token: "a" },
      { kind: "bogus" },
      { kind: "progress" },
      { kind: "cancellation" },
    ],
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
    await invoke(
      "AdapterDatabaseSession",
      "capabilities",
      W.AdapterDatabaseSessionCapabilitiesParamsCodec,
      {},
      {
        target: id,
      },
    ),
    W.AdapterDatabaseSessionCapabilitiesResultsCodec,
  );
  assert.equal(caps.kind, "ok");
  assert.equal(caps.value.sqlContractVersion, 1);
  assert.equal(caps.value.atomicBatch, true);
  assert.equal(caps.value.schemaMigrations, false);
  assert.equal(
    caps.value.maxStreamedResultBytes,
    262_144,
    "an isolate adapter's streamed budget is clamped to one scalar",
  );
  assert.equal(caps.value.maxStreamedResultRows, 250_000, "the row budget is kept");

  // executeStreamed: frames within one scalar travel inline, frames past it
  // are a typed limit error, and anything but a Uint8Array is an internal
  // error (never encoded from a look-alike's `length`).
  const streamedRequest = (operationId) => ({
    request: {
      operationId,
      requestHash: "",
      statements: [],
      deadlineUnixMs: 0,
      isolation: "atomicBatch",
      receipt: { guestLen: 0, guestHash: "" },
    },
  });
  const inline = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "executeStreamed",
      W.AdapterDatabaseSessionExecuteStreamedParamsCodec,
      streamedRequest("frames-small"),
      { target: id },
    ),
    W.AdapterDatabaseSessionExecuteStreamedResultsCodec,
  );
  assert.equal(inline.kind, "inline");
  assert.deepEqual(Array.from(inline.value), [1, 2, 3, 4]);
  assert.equal(calls.streamed.operationId, "frames-small");
  assert.equal(calls.streamed.isolation, "atomicBatch");
  const tooBig = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "executeStreamed",
      W.AdapterDatabaseSessionExecuteStreamedParamsCodec,
      streamedRequest("frames-big"),
      { target: id },
    ),
    W.AdapterDatabaseSessionExecuteStreamedResultsCodec,
  );
  assert.equal(tooBig.kind, "err");
  assert.equal(tooBig.value.code, "limit_exceeded");
  assert.match(tooBig.value.message, /maxScalarBytes/);
  const lookAlike = await invoke(
    "AdapterDatabaseSession",
    "executeStreamed",
    W.AdapterDatabaseSessionExecuteStreamedParamsCodec,
    streamedRequest("frames-fake"),
    { target: id },
  );
  assert.equal(lookAlike.status, 500, "a non-Uint8Array reply is an internal error");

  const relations = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "listUserRelations",
      W.AdapterDatabaseSessionListUserRelationsParamsCodec,
      {},
      {
        target: id,
      },
    ),
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
    await invoke(
      "AdapterDatabaseSession",
      "bootstrap",
      W.AdapterDatabaseSessionBootstrapParamsCodec,
      {},
      { target: id },
    ),
    W.AdapterDatabaseSessionBootstrapResultsCodec,
  );
  assert.deepEqual(unsupported, {
    kind: "err",
    value: { code: "unsupported", message: "bootstrap not implemented", details: "" },
  });

  const noTarget = await invoke(
    "AdapterDatabaseSession",
    "close",
    W.AdapterDatabaseSessionCloseParamsCodec,
    {},
  );
  assert.equal(noTarget.status, 400, "session calls need X-Bookclerk-Target");

  const closed = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "close",
      W.AdapterDatabaseSessionCloseParamsCodec,
      {},
      { target: id },
    ),
    W.AdapterDatabaseSessionCloseResultsCodec,
  );
  assert.deepEqual(closed, { kind: "ok" });
  const gone = decodeReply(
    await invoke(
      "AdapterDatabaseSession",
      "capabilities",
      W.AdapterDatabaseSessionCapabilitiesParamsCodec,
      {},
      {
        target: id,
      },
    ),
    W.AdapterDatabaseSessionCapabilitiesResultsCodec,
  );
  assert.equal(gone.kind, "err");
  assert.equal(gone.value.code, "invalid_params", "closed sessions are released");
}

// --- PluginWorker.databaseMigrations (no context header) ------------------

{
  const result = decodeReply(
    await invoke(
      "PluginWorker",
      "databaseMigrations",
      W.PluginWorkerDatabaseMigrationsParamsCodec,
      { binding: "DB" },
      {
        ctx: null,
      },
    ),
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

// --- MetadataProvider -----------------------------------------------------

{
  const health = decodeReply(
    await invoke("MetadataProvider", "health", W.MetadataProviderHealthParamsCodec, {}),
    W.MetadataProviderHealthResultsCodec,
  );
  assert.equal(health.kind, "ok");
  assert.equal(health.value.ok, true);

  const found = decodeReply(
    await invoke("MetadataProvider", "lookup", W.MetadataProviderLookupParamsCodec, {
      params: { identifiers: [{ scheme: "isbn", value: "9780441172719" }] },
    }),
    W.MetadataProviderLookupResultsCodec,
  );
  assert.equal(found.kind, "ok");
  assert.equal(found.value.records.length, 1);
  const [record] = found.value.records;
  assert.equal(record.title, "Dune");
  assert.equal(record.mediaKind, "print");
  assert.deepEqual(
    record.identifiers.map((id) => id.scheme),
    ["isbn", "openLibraryWork"],
  );
  assert.equal(record.credits[0].name, "Frank Herbert");
  assert.equal(record.seriesEntries[0].position, "1");

  const none = decodeReply(
    await invoke("MetadataProvider", "lookup", W.MetadataProviderLookupParamsCodec, {
      params: { identifiers: [{ scheme: "asin", value: "B000000000" }] },
    }),
    W.MetadataProviderLookupResultsCodec,
  );
  assert.equal(none.kind, "ok");
  assert.deepEqual(none.value.records, []);

  const searched = decodeReply(
    await invoke("MetadataProvider", "search", W.MetadataProviderSearchParamsCodec, {
      params: { title: "Dune", author: "Herbert" },
    }),
    W.MetadataProviderSearchResultsCodec,
  );
  assert.equal(searched.value.records[0].title, "Dune by Herbert");

  const wrapped = decodeReply(
    await invoke("MetadataProvider", "search", W.MetadataProviderSearchParamsCodec, {
      params: { title: "wrapped" },
    }),
    W.MetadataProviderSearchResultsCodec,
  );
  assert.equal(wrapped.kind, "err", "a non-array return is an error, not an empty answer");
  assert.equal(wrapped.value.code, "internal");

  const chapters = decodeReply(
    await invoke("MetadataProvider", "chapters", W.MetadataProviderChaptersParamsCodec, {
      params: { identifiers: [{ scheme: "asin", value: "B002V1OF70" }] },
    }),
    W.MetadataProviderChaptersResultsCodec,
  );
  assert.equal(chapters.kind, "ok");
  assert.equal(chapters.value.found, true);
  assert.equal(chapters.value.accurate, true);
  assert.equal(chapters.value.brandIntroMs, 2000);
  assert.deepEqual(
    chapters.value.chapters.map((c) => [c.title, c.depth]),
    [
      ["Book One", 0],
      ["Chapter 1", 1],
    ],
  );

  const missing = decodeReply(
    await invoke("MetadataProvider", "chapters", W.MetadataProviderChaptersParamsCodec, {
      params: { identifiers: [{ scheme: "asin", value: "B000000000" }] },
    }),
    W.MetadataProviderChaptersResultsCodec,
  );
  assert.equal(missing.value.found, false);
}

// --- CoverFetcher --------------------------------------------------------

{
  const health = decodeReply(
    await invoke("CoverFetcher", "health", W.CoverFetcherHealthParamsCodec, {}),
    W.CoverFetcherHealthResultsCodec,
  );
  assert.deepEqual(health, { kind: "ok", value: { ok: true, detail: "" } });

  granted.requests.length = 0;
  granted.puts = [];
  const fetched = decodeReply(
    await invoke(
      "CoverFetcher",
      "fetchCovers",
      W.CoverFetcherFetchCoversParamsCodec,
      {
        params: {
          urls: ["https://a.example/1.jpg", "https://a.example/missing.jpg", "https://b.example/3"],
          maxBytes: 1024,
        },
        sink: "sink",
      },
      { caps: [{ kind: "destination", token: "tok-sink" }] },
    ),
    W.CoverFetcherFetchCoversResultsCodec,
  );
  assert.equal(fetched.kind, "ok");
  assert.deepEqual(
    fetched.value.images.map((i) => [i.url, i.stored, i.error ?? ""]),
    [
      ["https://a.example/1.jpg", true, ""],
      ["https://a.example/missing.jpg", false, "HTTP 404"],
      ["https://b.example/3", true, ""],
    ],
  );
  assert.deepEqual(
    granted.puts.map((p) => [p.key, new TextDecoder().decode(p.bytes)]),
    [
      ["0", "img:0"],
      ["2", "img:2"],
    ],
    "bodies go to the sink under their index",
  );
  const put = granted.requests.find((r) => r.path === "/put");
  assert.equal(put.headers.Authorization, "Bearer tok-sink");
  assert.equal(put.headers["content-type"], "image/jpeg");

  const wrapped = decodeReply(
    await invoke(
      "CoverFetcher",
      "fetchCovers",
      W.CoverFetcherFetchCoversParamsCodec,
      { params: { urls: ["https://wrapped.example/"], maxBytes: 1 }, sink: "sink" },
      { caps: [{ kind: "destination", token: "tok-sink" }] },
    ),
    W.CoverFetcherFetchCoversResultsCodec,
  );
  assert.equal(wrapped.kind, "err");
  assert.equal(wrapped.value.code, "internal");
  assert.match(wrapped.value.message, /CoverImage\[\]/);

  const noSink = decodeReply(
    await invoke("CoverFetcher", "fetchCovers", W.CoverFetcherFetchCoversParamsCodec, {
      params: { urls: ["https://a.example/1.jpg"], maxBytes: 1 },
    }),
    W.CoverFetcherFetchCoversResultsCodec,
  );
  assert.equal(noSink.kind, "err");
  assert.equal(noSink.value.code, "invalid_params");
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
  const streamRoute = await invoke("Destination", "get", W.DestinationHeadParamsCodec, {
    key: "k",
  });
  assert.equal(streamRoute.status, 400, "get/put stay stream routes");

  const missingBinding = await invoke(
    "RemoteLibrary",
    "health",
    W.RemoteLibraryHealthParamsCodec,
    {},
  );
  assert.equal(missingBinding.status, 404);
  assert.equal(missingBinding.error.code, "unsupported");

  const malformed = await adapter.invoke(
    "ContentSource",
    "health",
    contextJson,
    null,
    null,
    new Uint8Array([1, 2, 3]),
  );
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
  const embedAdapter = new EmbedAdapter(
    {},
    { ...env, PLUGIN_STOREFRONT: new embed.StorefrontEntrypoint({}, {}) },
  );
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
