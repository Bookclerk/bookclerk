/**
 * Tests the object-store destination helpers and the queue change feed
 * against an in-memory backend, a fake queue and a fake KV binding. Key
 * translation runs the vectors the Rust storage helpers and the Python SDK
 * check too.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const store = await import(join(dist, "object-store.js"));
const feedModule = await import(join(dist, "queue-feed.js"));
const { PluginError } = await import(join(dist, "errors.js"));
const {
  ObjectStoreDestination,
  NO_FEED_CURSOR,
  checkByteRange,
  clampPageLimit,
  parseHostKey,
  requireDisjointPrefixes,
  stageKey,
  translateForObjectStore,
  writeCondition,
} = store;
const { QueueFeed, orderChanges, parseQueueCursor, webhookQueue } = feedModule;
const { createTestKv, createTestWebhooks } = await import(join(dist, "testkit-bindings.js"));

/**
 * Asserts `run` throws (or rejects with) a plugin error of `code`.
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

// --- Key translation vectors shared with Rust and Python -------------------

const vectors = JSON.parse(readFileSync(join(root, "fixtures/object-store-keys.json"), "utf8"));
assert.ok(vectors.length > 10);
for (const { key, prefixBytes, expected } of vectors) {
  assert.equal(translateForObjectStore(parseHostKey(key), prefixBytes), expected, key);
}
for (const bad of [
  "",
  "/a",
  "a/",
  "a//b",
  "./a",
  "a/../b",
  "a\\b",
  "a\u0000b",
  "a\nb",
  "a\u0085b",
]) {
  await rejectsWith(() => parseHostKey(bad), "invalid_params");
}
await rejectsWith(() => parseHostKey("a".repeat(1025)), "invalid_params");
assert.equal(parseHostKey("Author: Name/Title?/Book.m4b"), "Author: Name/Title?/Book.m4b");

// --- Small checks -----------------------------------------------------------

assert.equal(checkByteRange(undefined), undefined);
await rejectsWith(() => checkByteRange({ offset: 0, length: 0 }), "invalid_params");
await rejectsWith(() => checkByteRange({ offset: 5, length: 6 }, 10), "invalid_params");
await rejectsWith(() => checkByteRange({ offset: 11 }, 10), "invalid_params");
await rejectsWith(() => checkByteRange({ offset: -1 }), "invalid_params");
assert.deepEqual(checkByteRange({ offset: 10 }, 10), { offset: 10 });
assert.equal(clampPageLimit(0), 256);
assert.equal(clampPageLimit(undefined), 256);
assert.equal(clampPageLimit(9999), 256);
assert.equal(clampPageLimit(7), 7);
assert.equal(stageKey("tok", "a/b"), ".bookclerk-stage/tok/a/b");
for (const bad of ["", "a/b", "a\\b", "a..b", "a\u0000"])
  await rejectsWith(() => stageKey(bad, "k"), "invalid_params");
await rejectsWith(() => stageKey(undefined, "k"), "invalid_params");
await rejectsWith(() => stageKey("t", ""), "invalid_params");
assert.equal(writeCondition({}), null);
assert.deepEqual(writeCondition({ ifMatch: "e" }), { kind: "ifMatch", etag: "e" });
assert.deepEqual(writeCondition({ ifNoneMatch: true }), { kind: "ifNoneMatch" });
await rejectsWith(() => writeCondition({ ifMatch: "e", ifNoneMatch: true }), "invalid_params");
await rejectsWith(() => writeCondition({ ifNoneMatch: true, stageOnly: true }), "invalid_params");
await rejectsWith(() => requireDisjointPrefixes("", "b/"), "invalid_params");
await rejectsWith(() => requireDisjointPrefixes("a/", "a/b/"), "invalid_params");
await rejectsWith(() => requireDisjointPrefixes("./a/b/", "a\\"), "invalid_params");
requireDisjointPrefixes("a/", "b/");

// --- In-memory backend ------------------------------------------------------

/**
 * Collects a stream into bytes.
 *
 * @param {ReadableStream<Uint8Array>} body - The stream.
 * @returns {Promise<Uint8Array>} Its bytes.
 */
async function collect(body) {
  const chunks = [];
  for await (const chunk of body) chunks.push(chunk);
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

/**
 * A stream of `bytes`.
 *
 * @param {Uint8Array|string} bytes - The body.
 * @returns {ReadableStream<Uint8Array>} The stream.
 */
function streamOf(bytes) {
  const data = typeof bytes === "string" ? new TextEncoder().encode(bytes) : bytes;
  return new ReadableStream({
    start(controller) {
      controller.enqueue(data);
      controller.close();
    },
  });
}

/**
 * A backend over a `Map`, counting calls.
 *
 * @param {object} options - `serverCopy` and `conditionalWrites`.
 * @returns {object} The backend.
 */
function memoryStore(options = {}) {
  const objects = new Map();
  let version = 0;
  const calls = [];
  const backend = {
    objects,
    calls,
    serverCopy: options.serverCopy ?? true,
    prefixBytes: 8,
    async head(key) {
      calls.push(`head ${key}`);
      const found = objects.get(key);
      return found === undefined ? null : { ...found.meta };
    },
    async list(prefix, cursor, limit) {
      const keys = [...objects.keys()]
        .filter((key) => key.startsWith(prefix) && (cursor === undefined || key > cursor))
        .sort();
      const page = keys.slice(0, limit);
      const out = { objects: page.map((key) => ({ key, size: objects.get(key).bytes.length })) };
      if (keys.length > limit) out.nextCursor = page.at(-1);
      return out;
    },
    async get(key) {
      const found = objects.get(key);
      if (found === undefined) throw PluginError.fromWire("not_found", key);
      return { object: { ...found.meta }, body: streamOf(found.bytes) };
    },
    async put(key, body, write) {
      calls.push(`put ${key}`);
      const bytes = await collect(body);
      version += 1;
      const meta = { key, size: bytes.length, etag: `v${version}` };
      if (write.contentType !== undefined) meta.contentType = write.contentType;
      if (write.commitToken !== undefined) meta.commitToken = write.commitToken;
      objects.set(key, { bytes, meta });
      return { bytesWritten: bytes.length, etag: meta.etag };
    },
    async copy(from, to) {
      calls.push(`copy ${from} ${to}`);
      const found = objects.get(from);
      if (found === undefined) throw PluginError.fromWire("not_found", from);
      version += 1;
      objects.set(to, {
        bytes: found.bytes,
        meta: { ...found.meta, key: to, etag: `v${version}` },
      });
      return found.bytes.length;
    },
    async delete(key) {
      calls.push(`delete ${key}`);
      objects.delete(key);
    },
  };
  if (options.conditionalWrites !== undefined) {
    backend.conditionalWrites = options.conditionalWrites;
    backend.putConditional = async (key, body, write, condition) => {
      calls.push(`putConditional ${key} ${condition.kind}`);
      const current = objects.get(key);
      if (condition.kind === "ifNoneMatch" && current !== undefined) {
        await body.cancel();
        throw PluginError.fromWire("conflict", `${key} already exists`);
      }
      if (condition.kind === "ifMatch" && current?.meta.etag !== condition.etag) {
        await body.cancel();
        throw PluginError.fromWire("conflict", `${key} changed`);
      }
      return backend.put(key, body, write);
    };
  }
  return backend;
}

// Staged writes commit idempotently.
{
  const backend = memoryStore();
  const dest = new ObjectStoreDestination(backend);
  const put = await dest.put("a/b.m4b", streamOf("hello"), {
    commitToken: "tok",
    stageOnly: true,
    contentType: "audio/mp4",
  });
  assert.deepEqual(put, { key: "a/b.m4b", bytesWritten: 5, etag: "tok" });
  assert.ok(backend.objects.has(".bookclerk-stage/tok/a/b.m4b"));
  assert.equal(await dest.head("a/b.m4b"), null);
  const committed = await dest.commit("a/b.m4b", "tok");
  assert.deepEqual(committed, { key: "a/b.m4b", bytesWritten: 5, etag: "tok" });
  assert.ok(!backend.objects.has(".bookclerk-stage/tok/a/b.m4b"));
  assert.equal(new TextDecoder().decode(backend.objects.get("a/b.m4b").bytes), "hello");
  // A second commit after the stage is gone succeeds on the same token.
  assert.deepEqual(await dest.commit("a/b.m4b", "tok"), committed);
  await rejectsWith(() => dest.commit("a/b.m4b", "other"), "not_found");
  await rejectsWith(() => dest.commit("missing", "tok"), "not_found");
  // Aborting a stage that is gone is fine; the head hides the token.
  await dest.abortStage("a/b.m4b", "tok");
  const head = await dest.head("a/b.m4b");
  assert.equal(head.commitToken, undefined);
  assert.equal(head.contentType, "audio/mp4");
  await rejectsWith(() => dest.put("k", streamOf("x"), { sha256: new Uint8Array(3) }), "internal");
}

// Conditional writes: the backend's own, or a probe first.
for (const conditionalWrites of [undefined, "checked"]) {
  const backend = memoryStore({ conditionalWrites });
  const dest = new ObjectStoreDestination(backend);
  await dest.put("k", streamOf("1"), { ifNoneMatch: true });
  await rejectsWith(() => dest.put("k", streamOf("2"), { ifNoneMatch: true }), "conflict");
  const etag = (await dest.head("k")).etag;
  await dest.put("k", streamOf("3"), { ifMatch: etag });
  await rejectsWith(() => dest.put("k", streamOf("4"), { ifMatch: etag }), "conflict");
  assert.equal(
    backend.calls.some((call) => call.startsWith("putConditional")),
    conditionalWrites !== undefined,
  );
}

// Features follow how the backend keeps conditions.
assert.deepEqual(ObjectStoreDestination.features({}), ["storage.copy", "storage.translateKeys"]);
assert.deepEqual(
  ObjectStoreDestination.features({ conditionalWrites: "checked" }, { changes: true }),
  ["storage.copy", "storage.translateKeys", "storage.checkedWrite", "storage.changes"],
);
assert.ok(
  ObjectStoreDestination.features({ conditionalWrites: "atomic" }).includes(
    "storage.conditionalWrite",
  ),
);

// Moves: server copy or streamed, the overwrite rule, and the source check.
for (const serverCopy of [true, false]) {
  const backend = memoryStore({ serverCopy });
  const dest = new ObjectStoreDestination(backend);
  await dest.put("from", streamOf("abc"), {});
  assert.deepEqual(await dest.move("from", "to", {}), { atomic: false, alreadyMoved: false });
  assert.ok(!backend.objects.has("from"));
  assert.equal(backend.calls.includes("copy from to"), serverCopy);
  assert.deepEqual(await dest.move("from", "to", {}), { atomic: false, alreadyMoved: true });
  await rejectsWith(() => dest.move("nope", "nope2", {}), "not_found");
  assert.deepEqual(await dest.move("to", "to", {}), { atomic: true, alreadyMoved: false });
  await dest.put("other", streamOf("z"), {});
  await rejectsWith(() => dest.move("to", "other", { noOverwrite: true }), "conflict");
  await rejectsWith(() => dest.move("to", "x", { ifSourceMatch: "stale" }), "conflict");
}
{
  // A checked-write backend keeps the overwrite rule through its own condition.
  const backend = memoryStore({ conditionalWrites: "checked" });
  const dest = new ObjectStoreDestination(backend);
  await dest.put("s", streamOf("1"), {});
  await dest.put("t", streamOf("2"), {});
  await rejectsWith(() => dest.move("s", "t", { noOverwrite: true }), "conflict");
  assert.ok(backend.calls.includes("putConditional t ifNoneMatch"));
}

// Batches: pairs, a prefix page, deletes and probes.
{
  const backend = memoryStore();
  const dest = new ObjectStoreDestination(backend);
  for (const key of ["p/a", "p/b", "p/c"]) await dest.put(key, streamOf(key), {});
  const pairs = await dest.moveMany({
    target: {
      kind: "pairs",
      value: [
        { from: "p/a", to: "q/a" },
        { from: "nope", to: "q/n" },
      ],
    },
  });
  assert.equal(pairs.objectsMoved, 1);
  assert.deepEqual(pairs.results[0], { key: "p/a" });
  assert.equal(pairs.results[1].error.code, "not_found");
  const first = await dest.moveMany({
    target: { kind: "prefix", value: { from: "p/", to: "r/" } },
    limit: 1,
  });
  assert.deepEqual(first.results, [{ key: "p/b" }]);
  assert.equal(first.nextCursor, "p/b");
  const second = await dest.moveMany({
    target: { kind: "prefix", value: { from: "p/", to: "r/" } },
    cursor: first.nextCursor,
    limit: 1,
  });
  assert.deepEqual(second.results, [{ key: "p/c" }]);
  assert.equal(second.nextCursor, undefined);
  assert.deepEqual([...backend.objects.keys()].sort(), ["q/a", "r/b", "r/c"]);
  await rejectsWith(
    () => dest.moveMany({ target: { kind: "prefix", value: { from: "r/", to: "r/x/" } } }),
    "invalid_params",
  );
  await rejectsWith(
    () =>
      dest.moveMany({
        target: {
          kind: "pairs",
          value: Array.from({ length: 257 }, () => ({ from: "a", to: "b" })),
        },
      }),
    "invalid_params",
  );
  assert.deepEqual(await dest.deleteMany(["q/a", "gone"]), [{ key: "q/a" }, { key: "gone" }]);
  const probes = await dest.headMany(["r/b", "gone"]);
  assert.equal(probes[0].key, "r/b");
  assert.equal(probes[1], null);
  await rejectsWith(() => dest.headMany(Array(257).fill("k")), "invalid_params");
  await rejectsWith(() => dest.deleteMany(Array(257).fill("k")), "invalid_params");
  assert.deepEqual(await dest.translateKeys(["A/B{1}.mp3"]), ["A/B_1_.mp3"]);
  await rejectsWith(() => dest.translateKeys(["a//b"]), "invalid_params");
  const listed = await dest.list({ prefix: "r/", limit: 1 });
  assert.deepEqual(listed, {
    objects: [{ key: "r/b", size: 3 }],
    nextCursor: "r/b",
    signaturesIncluded: false,
    modifiedSinceApplied: false,
  });
  const read = await dest.get("r/c");
  assert.equal(new TextDecoder().decode(await collect(read.body)), "p/c");
  await rejectsWith(() => dest.get("r/c", { range: { offset: 0, length: 0 } }), "invalid_params");
  const copied = await dest.copy("r/c", "r/d");
  assert.deepEqual(copied, { bytesCopied: 3 });
  await dest.delete("r/d");
  assert.ok(!backend.objects.has("r/d"));
  // With no feed the host lists.
  assert.deepEqual(await dest.changes({}), {
    changes: [],
    cursor: NO_FEED_CURSOR,
    more: false,
    complete: false,
  });
}

// --- Queue feed --------------------------------------------------------------

assert.deepEqual(parseQueueCursor("q1.abc.7"), { feed: "abc", seq: 7 });
for (const bad of ["", "q2.abc.1", "q1..1", "q1.abc.", "q1.abc.x", "q1.abc"]) {
  await rejectsWith(() => parseQueueCursor(bad), "invalid_cursor");
}
{
  const events = [
    { change: { key: "a", kind: "changed" }, time: "2", sequencer: "0B" },
    { change: { key: "b", kind: "changed" }, time: null, sequencer: null },
    { change: { key: "a", kind: "changed" }, time: "1", sequencer: "A" },
    { change: { key: "a", kind: "changed" }, time: "3", sequencer: "9" },
  ];
  orderChanges(events);
  assert.deepEqual(
    events.map((e) => `${e.change.key}${e.sequencer ?? "-"}`),
    ["b-", "a9", "aA", "a0B"],
  );
}

/**
 * Parses a test body: comma-separated keys, `!` for unreadable.
 *
 * @param {Uint8Array} body - The body.
 * @returns {object[]} Changes.
 */
function parse(body) {
  const text = new TextDecoder().decode(body);
  if (text === "!") throw new Error("unreadable");
  return text
    .split(",")
    .map((key) => ({ change: { key, kind: "changed" }, time: null, sequencer: null }));
}

{
  const hooks = createTestWebhooks();
  const kv = createTestKv();
  const feed = new QueueFeed({
    queue: webhookQueue(hooks, "changes"),
    parse,
    kv,
    stateKey: "feed:1",
  });
  const dest = new ObjectStoreDestination(memoryStore(), feed);
  hooks.deliver("changes", "a");
  hooks.deliver("changes", "b,c");
  // No cursor: the current position only, incomplete, nothing taken.
  const start = await dest.changes({ cursor: NO_FEED_CURSOR });
  assert.deepEqual(start.changes, []);
  assert.equal(start.complete, false);
  assert.equal(hooks.waiting("changes"), 2);
  // The next page takes both messages; a gap at the start keeps it incomplete.
  const one = await dest.changes({ cursor: start.cursor });
  assert.deepEqual(
    one.changes.map((c) => c.key),
    ["a", "b", "c"],
  );
  assert.equal(one.complete, true, "the start page is the gap, and this page is past it");
  assert.deepEqual(hooks.acked("changes"), []);
  // Confirming the page acknowledges its messages.
  const two = await dest.changes({ cursor: one.cursor });
  assert.deepEqual(two.changes, []);
  assert.equal(two.complete, true);
  assert.deepEqual(hooks.acked("changes").sort(), ["lease-1", "lease-2"]);
  // An older cursor of this feed releases what came after it.
  hooks.deliver("changes", "d");
  const three = await dest.changes({ cursor: two.cursor });
  assert.deepEqual(
    three.changes.map((c) => c.key),
    ["d"],
  );
  const replay = await dest.changes({ cursor: two.cursor });
  assert.deepEqual(
    replay.changes.map((c) => c.key),
    ["d"],
    "a page never confirmed is offered again",
  );
  assert.ok(hooks.retried("changes").length >= 1);
  // A cursor ahead of the feed is refused; another feed's cursor starts over.
  await rejectsWith(
    () => dest.changes({ cursor: `${replay.cursor.split(".").slice(0, 2).join(".")}.999` }),
    "invalid_cursor",
  );
  const restart = await dest.changes({ cursor: "q1.otherfeed.3" });
  assert.equal(restart.complete, false);
  // Unreadable messages and a missed flag make the page incomplete.
  hooks.deliver("changes", "!");
  const bad = await dest.changes({ cursor: restart.cursor });
  assert.equal(bad.complete, false);
  hooks.markMissed("changes");
  const missed = await dest.changes({ cursor: bad.cursor });
  assert.equal(missed.complete, false);
  // The page after a gap is complete again.
  const after = await dest.changes({ cursor: missed.cursor });
  assert.equal(after.complete, true);
  // State survives a new feed object (the isolate slept).
  const again = new QueueFeed({
    queue: webhookQueue(hooks, "changes"),
    parse,
    kv,
    stateKey: "feed:1",
  });
  const resumed = await again.changes(after.cursor, 0);
  assert.equal(resumed.complete, true);
}
{
  // Page limits: messages beyond the page are released, and one oversized
  // message is cut to the page and marked incomplete.
  const hooks = createTestWebhooks();
  const kv = createTestKv();
  const feed = new QueueFeed({
    queue: webhookQueue(hooks, "changes"),
    parse,
    kv,
    stateKey: "feed:2",
  });
  const start = await feed.changes(undefined, 0);
  hooks.deliver("changes", "a,b");
  hooks.deliver("changes", "c,d");
  const page = await feed.changes(start.cursor, 3);
  assert.deepEqual(
    page.changes.map((c) => c.key),
    ["a", "b"],
  );
  assert.equal(page.more, true);
  // Drop the released message so the next page starts with one big one.
  const left = await hooks.pull("changes", { batchSize: 10 });
  await hooks.ack("changes", { acks: left.messages.map((m) => m.leaseId) });
  hooks.deliver("changes", "w,x,y,z");
  const cut = await feed.changes(page.cursor, 3);
  assert.deepEqual(
    cut.changes.map((c) => c.key),
    ["w", "x", "y"],
  );
  assert.equal(cut.complete, false);
  // A KV that cannot save releases what the page took and fails the call.
  hooks.deliver("changes", "q");
  kv.failNext("put", PluginError.fromWire("internal", "kv down"));
  await rejectsWith(() => feed.changes(cut.cursor, 3), "internal");
}

console.log("object-store: ok");
