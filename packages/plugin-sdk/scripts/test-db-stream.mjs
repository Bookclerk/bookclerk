import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");
const corpusDir = join(root, "../../../fuzz/corpus/db_stream");

const {
  ExecuteStreamDecoder,
  collectExecuteStream,
  decodeExecuteStream,
  executeStreamLimits,
  rawStreamCeiling,
  streamChunks,
  streamsResults,
} = await import(join(dist, "db-stream.js"));
const { createDatabaseBinding, chunkIndexes, encodeExecuteRequest } = await import(
  join(dist, "db-execute.js")
);
const { ExecuteStreamFrameCodec, decodeMessage, encodeMessage } = await import(
  join(dist, "generated-wire.js")
);
const { MAX_SCALAR_BYTES, MAX_STREAM_WINDOW_BYTES } = await import(join(dist, "abi.js"));
const { CapnpReader } = await import(join(dist, "db-capnp.js"));
const { PluginError } = await import(join(dist, "errors.js"));

// The first-party sqlite advertisement as the host reads it (every value
// mirrors `DbCapabilities::advertised_sqlite()`).
const sqliteCaps = {
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
  maxPayloadBytes: MAX_SCALAR_BYTES,
  maxResultBytes: MAX_SCALAR_BYTES,
  maxCellBytes: MAX_SCALAR_BYTES,
  maxRequestBytes: MAX_SCALAR_BYTES,
  maxAtomicResultBytes: MAX_SCALAR_BYTES,
  maxStreamedResultBytes: 64 * 1024 * 1024,
  maxStreamedResultRows: 250_000,
  pluginDatabases: true,
  maxFunctionArgs: 127,
  maxSchemaColumns: 2000,
  maxPatternBytes: 50000,
  maxLoweredStatementBytes: 0,
  consistentBackupRead: true,
};
const inlineOnlyCaps = { ...sqliteCaps, maxStreamedResultBytes: 0, maxStreamedResultRows: 0 };

assert.equal(streamsResults(sqliteCaps), true);
assert.equal(streamsResults(inlineOnlyCaps), false);
assert.deepEqual(executeStreamLimits(sqliteCaps), {
  maxBytes: 64 * 1024 * 1024,
  bytesLimit: "maxStreamedResultBytes",
  maxRows: 250_000,
  rowsLimit: "maxStreamedResultRows",
});
assert.deepEqual(executeStreamLimits(inlineOnlyCaps), {
  maxBytes: MAX_SCALAR_BYTES,
  bytesLimit: "maxAtomicResultBytes",
  maxRows: 10000,
  rowsLimit: "maxResultRows",
});
const tinyLimits = {
  maxBytes: 4096,
  bytesLimit: "maxStreamedResultBytes",
  maxRows: 4,
  rowsLimit: "maxStreamedResultRows",
};
assert.equal(rawStreamCeiling(tinyLimits), 4096 + 512 + MAX_STREAM_WINDOW_BYTES);

// --- Shared vectors: the `db_stream` fuzz corpus -------------------------
//
// Each seed is a budget byte (0 = tiny 4096 B / 4 rows, 1 = first-party
// sqlite, 2 = an adapter that does not stream), a chunk-size byte (chunk =
// byte + 1) and the frame bytes. The Rust oracle (`fuzz/oracles/db_stream.rs`)
// checks the same invariants against the host's decoder.
function limitsFor(selector) {
  switch (selector % 3) {
    case 0:
      return tinyLimits;
    case 1:
      return executeStreamLimits(sqliteCaps);
    default:
      return executeStreamLimits(inlineOnlyCaps);
  }
}

function decodeOutcome(fn) {
  try {
    return { ok: fn() };
  } catch (err) {
    assert.ok(err && typeof err.wireCode === "string", `typed error expected, got ${err}`);
    return { err };
  }
}

function decodeChunked(stream, chunk, limits) {
  const decoder = new ExecuteStreamDecoder(limits);
  for (let at = 0; at < stream.byteLength; at += chunk) {
    decoder.push(stream.subarray(at, Math.min(at + chunk, stream.byteLength)));
  }
  return decoder.finish();
}

const EXPECTED = {
  "reply_fits.bin": { rows: 3 },
  "reply_fits_tiny_chunks.bin": { rows: 3 },
  "inline_caps.bin": { rows: 3 },
  "adapter_error.bin": { code: "unavailable" },
  "rows_over_tiny_cap.bin": { code: "limit_exceeded", limit: "maxStreamedResultRows" },
  "truncated.bin": { code: "unavailable" },
  "bytes_after_end.bin": { code: "invalid_params" },
};

const seeds = readdirSync(corpusDir).filter((name) => name.endsWith(".bin"));
assert.ok(seeds.length >= Object.keys(EXPECTED).length, "db_stream corpus is incomplete");
for (const name of seeds) {
  const data = new Uint8Array(readFileSync(join(corpusDir, name)));
  const limits = limitsFor(data[0]);
  const chunk = data[1] + 1;
  const stream = data.subarray(2);

  const whole = decodeOutcome(() => decodeExecuteStream(stream, limits));
  const split = decodeOutcome(() => decodeChunked(stream, chunk, limits));
  if ("ok" in whole) {
    assert.ok("ok" in split, `${name}: chunks of ${chunk} refused what one push accepted`);
    assert.deepEqual(split.ok, whole.ok, `${name}: chunked decode must agree with one push`);
    if (limits.maxRows > 0) {
      for (const stmt of whole.ok.statements) {
        assert.ok(stmt.rows.length <= limits.maxRows, `${name}: rows over the cap`);
      }
    }
  } else {
    assert.ok("err" in split, `${name}: chunks of ${chunk} accepted what one push refused`);
    assert.equal(split.err.wireCode, whole.err.wireCode, `${name}: error codes differ`);
  }

  const expected = EXPECTED[name];
  if (expected === undefined) {
    continue; // A regression seed only has to hold the invariants.
  }
  if ("rows" in expected) {
    assert.ok("ok" in whole, `${name}: expected a reply, got ${whole.err}`);
    assert.equal(whole.ok.operationId, "op-stream");
    assert.equal(whole.ok.statements.length, 2);
    assert.equal(whole.ok.statements[0].rows.length, expected.rows);
    assert.equal(whole.ok.statements[1].rows.length, 0);
    assert.equal(whole.ok.statements[1].rowsAffected, 2);
    assert.deepEqual(
      whole.ok.statements[0].columns.map((c) => c.name),
      ["id", "title"],
    );
    assert.deepEqual(whole.ok.statements[0].rows[2].values, [
      { kind: "int64", value: 2n },
      { kind: "text", value: "Title 002" },
    ]);
    assert.deepEqual(whole.ok.timing, {
      attemptElapsedUs: 11,
      dbExecutionUs: 7,
      dbTimingSource: "sqlite_txn",
    });
  } else {
    assert.ok("err" in whole, `${name}: expected ${expected.code}, got a reply`);
    assert.equal(whole.err.wireCode, expected.code, `${name}: wrong error code`);
    if (expected.limit) {
      assert.equal(whole.err.limit.limit, expected.limit);
      assert.equal(whole.err.limit.cap, limits.maxRows);
    }
  }
}

// --- Frame helpers for the binding tests ---------------------------------
function frame(kind, value) {
  return encodeMessage(ExecuteStreamFrameCodec, { kind, value });
}
function concat(parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.byteLength, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.byteLength;
  }
  return out;
}
const timing = { attemptElapsedUs: 5, dbExecutionUs: 3, dbTimingSource: "test" };
const columns = [
  { name: "id", dbType: "int64" },
  { name: "title", dbType: "text" },
];
function row(i) {
  return {
    values: [
      { kind: "int64", value: BigInt(i) },
      { kind: "text", value: `Title ${i}` },
    ],
  };
}
function streamOf(operationId, rows, { rowsPerFrame = 2 } = {}) {
  const frames = [
    frame("head", { operationId, statementCount: 1 }),
    frame("statement", { index: 0, columns, rowsAffected: 0 }),
  ];
  for (let at = 0; at < rows.length; at += rowsPerFrame) {
    frames.push(frame("rows", rows.slice(at, at + rowsPerFrame)));
  }
  frames.push(frame("end", timing));
  return concat(frames);
}
function errStreamOf(operationId, err) {
  return concat([frame("head", { operationId, statementCount: 1 }), frame("err", err)]);
}
// A rows frame of `count` single-cell rows whose first cell carries an
// unknown union discriminant, so the codec refuses the frame if it decodes
// the cells. The discriminant is located by encoding the first cell as
// `int64 0` and as `boolean false`, which differ only in that field.
function rowsWithUnknownCell(count) {
  const rest = Array.from({ length: count - 1 }, () => ({
    values: [{ kind: "int64", value: 0n }],
  }));
  const asInt = frame("rows", [{ values: [{ kind: "int64", value: 0n }] }, ...rest]);
  const asBool = frame("rows", [{ values: [{ kind: "boolean", value: false }] }, ...rest]);
  assert.equal(asInt.byteLength, asBool.byteLength);
  const at = asInt.findIndex((byte, i) => byte !== asBool[i]);
  assert.ok(at >= 0, "the two encodings differ in the discriminant");
  const out = asInt.slice();
  out[at] = 0xff;
  out[at + 1] = 0xff;
  assert.throws(() => decodeMessage(ExecuteStreamFrameCodec, out), /union/);
  return out;
}
// A rows frame whose list is `count` zero-width rows: a composite list of
// zero words that still carries its tag (count, no data, no pointers).
function zeroWidthRows(count) {
  const words = [(1n << 32n) | (1n << 48n), 2n, 1n | (7n << 32n), BigInt(count) << 2n];
  const out = new Uint8Array(8 + words.length * 8);
  const view = new DataView(out.buffer);
  view.setUint32(0, 0, true);
  view.setUint32(4, words.length, true);
  words.forEach((w, i) => view.setBigUint64(8 + i * 8, w, true));
  return out;
}
function readable(bytes, size) {
  let at = 0;
  return new ReadableStream({
    pull(controller) {
      if (at >= bytes.byteLength) {
        controller.close();
        return;
      }
      controller.enqueue(bytes.slice(at, Math.min(at + size, bytes.byteLength)));
      at += size;
    },
  });
}

// `collectExecuteStream` reassembles async chunks.
{
  const rows = Array.from({ length: 5 }, (_, i) => row(i));
  const bytes = streamOf("op-collect", rows);
  const reply = await collectExecuteStream(
    (async function* () {
      yield bytes.subarray(0, 7);
      yield bytes.subarray(7, 7);
      yield bytes.subarray(7);
    })(),
    executeStreamLimits(sqliteCaps),
  );
  assert.equal(reply.operationId, "op-collect");
  assert.equal(reply.statements[0].rows.length, 5);
}

// A frame whose declared length alone passes the ceiling fails closed before
// any body bytes arrive.
{
  const decoder = new ExecuteStreamDecoder(tinyLimits);
  const header = new Uint8Array(8);
  new DataView(header.buffer).setUint32(4, 1 << 24, true);
  assert.throws(
    () => decoder.push(header),
    (err) => err.wireCode === "limit_exceeded" && err.limit.limit === "maxStreamedResultBytes",
  );
}

// `retainRows: false` still counts rows against the cap but keeps none.
{
  const decoder = new ExecuteStreamDecoder(tinyLimits, { retainRows: false });
  const rows = Array.from({ length: 3 }, (_, i) => row(i));
  const frames = decoder.push(streamOf("op-iter", rows));
  assert.deepEqual(
    frames.map((f) => f.kind),
    ["head", "statement", "rows", "rows", "end"],
  );
  assert.equal(decoder.finish().statements[0].rows.length, 0);
  const over = new ExecuteStreamDecoder(tinyLimits, { retainRows: false });
  assert.throws(
    () =>
      over.push(
        streamOf(
          "op-iter",
          Array.from({ length: 5 }, (_, i) => row(i)),
        ),
      ),
    (err) => err.wireCode === "limit_exceeded" && err.limit.limit === "maxStreamedResultRows",
  );
}

// --- Deterministic mutation sweep over the same seeds ---------------------
//
// The fuzzer runs against the Rust decoder; this sweep derives mutants of
// every seed with a fixed PRNG so each SDK decoder meets the same shapes on
// every run: a flipped byte, a truncation, an inserted byte and a duplicated
// slice. The invariants are the corpus invariants, not expected outcomes.
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
function mutate(stream, rand) {
  const below = (n) => Math.floor(rand() * n);
  const at = stream.byteLength === 0 ? 0 : below(stream.byteLength);
  switch (below(4)) {
    case 0: {
      const out = Uint8Array.from(stream);
      if (out.byteLength > 0) {
        out[at] ^= 1 << below(8);
      }
      return { kind: "flip", out };
    }
    case 1:
      return { kind: "truncate", out: stream.slice(0, at) };
    case 2:
      return {
        kind: "insert",
        out: concat([stream.slice(0, at), Uint8Array.of(below(256)), stream.slice(at)]),
      };
    default: {
      const end = at + below(Math.max(1, stream.byteLength - at) + 1);
      return {
        kind: "duplicate",
        out: concat([stream.slice(0, end), stream.slice(at, end), stream.slice(end)]),
      };
    }
  }
}
{
  const rand = mulberry32(0x5eed_d8);
  let mutants = 0;
  let accepted = 0;
  for (const name of seeds) {
    const data = new Uint8Array(readFileSync(join(corpusDir, name)));
    const limits = limitsFor(data[0]);
    for (let i = 0; i < 24; i += 1) {
      const { kind, out } = mutate(data.subarray(2), rand);
      const chunk = 1 + Math.floor(rand() * 64);
      const label = `${name} ${kind} #${i} (chunks of ${chunk})`;
      const whole = decodeOutcome(() => decodeExecuteStream(out, limits));
      const split = decodeOutcome(() => decodeChunked(out, chunk, limits));
      mutants += 1;
      if ("ok" in whole) {
        accepted += 1;
        assert.ok("ok" in split, `${label}: chunked decode refused what one push accepted`);
        assert.deepEqual(split.ok, whole.ok, `${label}: chunked decode must agree with one push`);
        if (limits.maxRows > 0) {
          for (const stmt of whole.ok.statements) {
            assert.ok(stmt.rows.length <= limits.maxRows, `${label}: rows over the cap`);
          }
        }
      } else {
        assert.ok("err" in split, `${label}: chunked decode accepted what one push refused`);
        assert.equal(split.err.wireCode, whole.err.wireCode, `${label}: error codes differ`);
      }
    }
  }
  assert.ok(mutants >= 24 * Object.keys(EXPECTED).length);
  assert.ok(accepted > 0, "some mutants still decode (the sweep is not all noise)");
}

// A frame carrying one wide row decodes under the stream's budget even when
// it is larger than the generic reader's budget, which still applies to
// control messages; many frames in one push are consumed in one pass.
{
  const wide = ["a", "b", "c"].map((name) => ({ name, dbType: "text" }));
  const cells = wide.map((c) => ({ kind: "text", value: c.name.repeat(400 * 1024) }));
  const bytes = concat([
    frame("head", { operationId: "op-wide", statementCount: 1 }),
    frame("statement", { index: 0, columns: wide, rowsAffected: 0 }),
    frame("rows", [{ values: cells }]),
    frame("end", timing),
  ]);
  assert.ok(bytes.byteLength > 1024 * 1024, "the row frame is wider than the 1 MiB window");
  const reply = decodeExecuteStream(bytes, executeStreamLimits(sqliteCaps));
  assert.equal(reply.statements[0].rows[0].values[2].value.length, 400 * 1024);
  assert.deepEqual(decodeChunked(bytes, 4096, executeStreamLimits(sqliteCaps)), reply);
  assert.throws(
    () => decodeMessage(ExecuteStreamFrameCodec, frame("rows", [{ values: cells }])),
    /traversal budget/,
  );
  // Under a budget the frame does not fit, the declared length fails closed.
  assert.throws(
    () => decodeExecuteStream(bytes, tinyLimits),
    (err) => err.wireCode === "limit_exceeded" && err.limit.limit === "maxStreamedResultBytes",
  );

  const many = Array.from({ length: 3000 }, (_, i) => row(i));
  const decoder = new ExecuteStreamDecoder(executeStreamLimits(sqliteCaps), { retainRows: false });
  const frames = decoder.push(streamOf("op-many", many, { rowsPerFrame: 1 }));
  assert.equal(frames.filter((f) => f.kind === "rows").length, 3000);
  assert.ok(decoder.finished);
}

// --- The binding reads through `executeStreamed` --------------------------
function transportOf({ caps, body, onInline, onStreamed }) {
  const calls = { inline: 0, streamed: 0, limits: 0 };
  const transport = {
    async execute(req) {
      calls.inline += 1;
      if (onInline) {
        return onInline(req);
      }
      throw new Error("inline execute must not be used");
    },
  };
  if (caps !== undefined) {
    transport.limits = async () => {
      calls.limits += 1;
      return caps;
    };
  }
  if (body !== undefined || onStreamed !== undefined) {
    transport.executeStreamed = async (req) => {
      calls.streamed += 1;
      if (onStreamed) {
        return onStreamed(req);
      }
      return body(req);
    };
  }
  return { transport, calls };
}
const inlineReply = (req, rows = []) => ({
  operationId: req.operationId,
  statements: req.statements.map(() => ({ rows, columns, rowsAffected: 0, cursor: "" })),
  timing,
});

// Whole frames in one scalar.
{
  const rows = Array.from({ length: 7 }, (_, i) => row(i));
  const { transport, calls } = transportOf({
    caps: sqliteCaps,
    body: (req) => streamOf(req.operationId, rows),
  });
  const db = createDatabaseBinding(transport);
  const result = await db.prepare("SELECT id, title FROM books").all();
  assert.equal(result.results.length, 7);
  assert.equal(result.results[6].title.value, "Title 6");
  await db.prepare("SELECT id, title FROM books").all();
  assert.deepEqual(calls, { inline: 0, streamed: 2, limits: 1 }, "limits are fetched once");
}

// A streamed body in small windows.
{
  const rows = Array.from({ length: 40 }, (_, i) => row(i));
  const { transport, calls } = transportOf({
    caps: sqliteCaps,
    body: (req) => readable(streamOf(req.operationId, rows, { rowsPerFrame: 7 }), 13),
  });
  const db = createDatabaseBinding(transport);
  const raw = await db.prepare("SELECT id, title FROM books").raw();
  assert.equal(raw.length, 40);
  assert.equal(raw[39][0].value, 39n);
  assert.deepEqual(calls, { inline: 0, streamed: 1, limits: 1 });
}

// Sessions that do not stream, hosts without `limits()` and transports
// without `executeStreamed` all read inline.
for (const setup of [
  { caps: inlineOnlyCaps, body: () => new Uint8Array(0) },
  { caps: undefined, body: () => new Uint8Array(0) },
  { caps: sqliteCaps },
]) {
  const { transport, calls } = transportOf({
    ...setup,
    onInline: (req) => inlineReply(req, [row(1)]),
  });
  const db = createDatabaseBinding(transport);
  const result = await db.prepare("SELECT id, title FROM books").all();
  assert.equal(result.results.length, 1);
  assert.equal(calls.streamed, 0);
  assert.equal(calls.inline, 1);
}

// `unsupported` from the transport or as the stream's `err` frame falls back
// to the inline read; every other error is the caller's.
{
  const { PluginError } = await import(join(dist, "errors.js"));
  const thrown = transportOf({
    caps: sqliteCaps,
    onStreamed: () => {
      throw PluginError.fromWire("unsupported", "GuestDatabase.executeStreamed");
    },
    onInline: (req) => inlineReply(req, [row(1), row(2)]),
  });
  const db = createDatabaseBinding(thrown.transport);
  assert.equal((await db.prepare("SELECT 1").all()).results.length, 2);
  assert.deepEqual(thrown.calls, { inline: 1, streamed: 1, limits: 1 });

  const framed = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      errStreamOf(req.operationId, { code: "unsupported", message: "no stream", details: "" }),
    onInline: (req) => inlineReply(req, [row(1)]),
  });
  const db2 = createDatabaseBinding(framed.transport);
  assert.equal((await db2.prepare("SELECT 1").all()).results.length, 1);
  assert.deepEqual(framed.calls, { inline: 1, streamed: 1, limits: 1 });

  const failing = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      errStreamOf(req.operationId, {
        code: "limit_exceeded",
        message: "limit exceeded: maxStreamedResultBytes is 67108864; used 70000000",
        details: "",
      }),
  });
  const db3 = createDatabaseBinding(failing.transport);
  await assert.rejects(
    db3.prepare("SELECT 1").all(),
    (err) =>
      err.code === "limit_exceeded" &&
      err.limit.limit === "maxStreamedResultBytes" &&
      err.limit.used === 70000000,
  );
  assert.equal(failing.calls.inline, 0);
}

// The decoder's own limit applies to what the transport hands back.
{
  const rows = Array.from({ length: 9 }, (_, i) => row(i));
  const tinyCaps = { ...sqliteCaps, maxStreamedResultBytes: 4096, maxStreamedResultRows: 4 };
  const { transport } = transportOf({
    caps: tinyCaps,
    body: (req) => streamOf(req.operationId, rows),
  });
  const db = createDatabaseBinding(transport);
  await assert.rejects(
    db.prepare("SELECT id, title FROM books").all(),
    (err) =>
      err.code === "limit_exceeded" &&
      err.limit.limit === "maxStreamedResultRows" &&
      err.limit.cap === 4,
  );
}

// `iterate()` yields rows as frames arrive and never holds the result.
{
  const rows = Array.from({ length: 25 }, (_, i) => row(i));
  const { transport, calls } = transportOf({
    caps: sqliteCaps,
    body: (req) => readable(streamOf(req.operationId, rows, { rowsPerFrame: 4 }), 50),
  });
  const db = createDatabaseBinding(transport);
  const seen = [];
  for await (const r of db.prepare("SELECT id, title FROM books").iterate()) {
    seen.push(r.id.value);
    assert.equal(r.title.kind, "text");
  }
  assert.deepEqual(
    seen,
    rows.map((_, i) => BigInt(i)),
  );
  assert.deepEqual(calls, { inline: 0, streamed: 1, limits: 1 });

  // Past the row cap the loop fails closed mid-way, with the typed error.
  const tinyCaps = { ...sqliteCaps, maxStreamedResultBytes: 4096, maxStreamedResultRows: 4 };
  const over = createDatabaseBinding(
    transportOf({ caps: tinyCaps, body: (req) => streamOf(req.operationId, rows) }).transport,
  );
  let count = 0;
  await assert.rejects(
    (async () => {
      for await (const _ of over.prepare("SELECT id FROM books").iterate()) {
        count += 1;
      }
    })(),
    (err) => err.code === "limit_exceeded" && err.limit.limit === "maxStreamedResultRows",
  );
  assert.ok(count <= 4);

  // The whole reply in one chunk completes every frame in one push.
  const whole = createDatabaseBinding(
    transportOf({ caps: sqliteCaps, body: (req) => streamOf(req.operationId, rows.slice(0, 3)) })
      .transport,
  );
  const wholeSeen = [];
  for await (const r of whole.prepare("SELECT id FROM books").iterate()) {
    wholeSeen.push(r.id.value);
  }
  assert.deepEqual(wholeSeen, [0n, 1n, 2n]);

  // Inline sessions iterate one `all()` reply.
  const inline = createDatabaseBinding(
    transportOf({ caps: inlineOnlyCaps, onInline: (req) => inlineReply(req, rows.slice(0, 3)) })
      .transport,
  );
  const inlineSeen = [];
  for await (const r of inline.prepare("SELECT id FROM books").iterate()) {
    inlineSeen.push(r.id.value);
  }
  assert.deepEqual(inlineSeen, [0n, 1n, 2n]);
}

// `iterate()` falls back to the inline read when the stream answers
// `unsupported` before any row; after a row it surfaces the error as is.
{
  const rows = [row(0), row(1), row(2)];
  const fell = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      errStreamOf(req.operationId, { code: "unsupported", message: "no stream", details: "" }),
    onInline: (req) => inlineReply(req, rows),
  });
  const seen = [];
  for await (const r of createDatabaseBinding(fell.transport).prepare("SELECT id").iterate()) {
    seen.push(r.id.value);
  }
  assert.deepEqual(seen, [0n, 1n, 2n]);
  assert.deepEqual(fell.calls, { inline: 1, streamed: 1, limits: 1 });

  // Chunked delivery takes the same path through `finish()`.
  const late = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      readable(
        errStreamOf(req.operationId, { code: "unsupported", message: "no stream", details: "" }),
        3,
      ),
    onInline: (req) => inlineReply(req, rows.slice(0, 1)),
  });
  const lateSeen = [];
  for await (const r of createDatabaseBinding(late.transport).prepare("SELECT id").iterate()) {
    lateSeen.push(r.id.value);
  }
  assert.deepEqual(lateSeen, [0n]);
  assert.deepEqual(late.calls, { inline: 1, streamed: 1, limits: 1 });

  const afterRows = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      concat([
        frame("head", { operationId: req.operationId, statementCount: 1 }),
        frame("statement", { index: 0, columns, rowsAffected: 0 }),
        frame("rows", rows),
        frame("err", { code: "unsupported", message: "gave up", details: "" }),
      ]),
    onInline: () => assert.fail("no inline fallback after rows were yielded"),
  });
  let count = 0;
  await assert.rejects(
    (async () => {
      for await (const _ of createDatabaseBinding(afterRows.transport)
        .prepare("SELECT id")
        .iterate()) {
        count += 1;
      }
    })(),
    (err) => err.code === "unsupported",
  );
  assert.equal(count, 3);
  assert.equal(afterRows.calls.inline, 0);

  // Other errors never fall back.
  const unavailable = transportOf({
    caps: sqliteCaps,
    body: (req) =>
      errStreamOf(req.operationId, { code: "unavailable", message: "down", details: "" }),
    onInline: () => assert.fail("only unsupported falls back"),
  });
  await assert.rejects(
    (async () => {
      for await (const _ of createDatabaseBinding(unavailable.transport)
        .prepare("SELECT id")
        .iterate()) {
        // unreachable
      }
    })(),
    (err) => err.code === "unavailable",
  );
}

// --- Reader and decoder hardening ------------------------------------------
// The traversal budget is cumulative: a message whose two pointers alias
// one large text charges it twice, so aliasing cannot amplify a frame.
{
  const textBytes = 8000; // 1000 words
  const words = 3 + textBytes / 8 + 1;
  const buf = new Uint8Array(8 + words * 8);
  const view = new DataView(buf.buffer);
  view.setUint32(4, words, true);
  const word = (i, v) => view.setBigUint64(8 + i * 8, v, true);
  word(0, 2n << 48n); // root struct at w1: 0 data words, 2 pointers
  const listPtr = (offset) =>
    1n | (BigInt(offset) << 2n) | (2n << 32n) | (BigInt(textBytes + 1) << 35n);
  word(1, listPtr(1)); // -> w3
  word(2, listPtr(0)); // -> w3 (the same text)
  buf.fill(0x61, 8 + 3 * 8, 8 + 3 * 8 + textBytes);
  const once = new CapnpReader(buf, words + 16).root(0, 2);
  assert.equal(once.getText(0).length, textBytes);
  const twice = new CapnpReader(buf, words + 16).root(0, 2);
  twice.getText(0);
  assert.throws(() => twice.getText(1), /traversal budget/);
  const roomy = new CapnpReader(buf, words * 2 + 16).root(0, 2);
  assert.equal(roomy.getText(1).length, textBytes);
}

// A failed push is latched: `finish()` reports the failure, not the reply
// the earlier frames described.
{
  const rows = [row(0)];
  const bytes = concat([streamOf("op-latch", rows), Uint8Array.of(0)]);
  const decoder = new ExecuteStreamDecoder(executeStreamLimits(sqliteCaps));
  assert.throws(
    () => decoder.push(bytes),
    (err) => err.wireCode === "invalid_params",
  );
  assert.ok(decoder.finished);
  assert.throws(
    () => decoder.finish(),
    (err) => err.wireCode === "invalid_params",
  );
}

// Streamed cells are validated like inline ones.
{
  for (const [cell, text] of [
    [{ kind: "float64", value: Number.NaN }, /not finite/],
    [{ kind: "text", value: "a\u0000b" }, /U\+0000/],
  ]) {
    const bytes = concat([
      frame("head", { operationId: "op-cell", statementCount: 1 }),
      frame("statement", { index: 0, columns: [{ name: "v", dbType: "text" }], rowsAffected: 0 }),
      frame("rows", [{ values: [cell] }]),
      frame("end", timing),
    ]);
    assert.throws(
      () => decodeExecuteStream(bytes, executeStreamLimits(sqliteCaps)),
      (err) => err.wireCode === "invalid_params" && text.test(err.message),
    );
  }
}

// The row cap is checked from a rows frame's count before its cells decode:
// a frame past the cap whose cells would not even validate is refused as
// `limit_exceeded`, and a rows frame ahead of any statement is refused as
// out of sequence, without materializing either.
{
  const bad = { kind: "text", value: "a\u0000b" };
  const head = frame("head", { operationId: "op-order", statementCount: 1 });
  const statement = frame("statement", {
    index: 0,
    columns: [{ name: "v", dbType: "int64" }],
    rowsAffected: 0,
  });
  // The frame's first cell does not even decode, so refusing it as
  // `limit_exceeded` shows the count was checked before any cell was read.
  const overCap = concat([head, statement, rowsWithUnknownCell(5)]);
  assert.throws(
    () => new ExecuteStreamDecoder(tinyLimits).push(overCap),
    (err) =>
      err.wireCode === "limit_exceeded" &&
      err.limit.limit === "maxStreamedResultRows" &&
      err.limit.used === 5,
  );
  // Under the cap the same frame is refused for what it is.
  assert.throws(
    () =>
      new ExecuteStreamDecoder(tinyLimits).push(concat([head, statement, rowsWithUnknownCell(3)])),
    (err) => err.wireCode === "invalid_params" && /does not decode/.test(err.message),
  );
  // Zero-width rows occupy no words, yet their count is read from the tag:
  // five of them pass the cap of four, three are rows of no cells.
  assert.throws(
    () => new ExecuteStreamDecoder(tinyLimits).push(concat([head, statement, zeroWidthRows(5)])),
    (err) => err.wireCode === "limit_exceeded" && err.limit.used === 5,
  );
  assert.throws(
    () => new ExecuteStreamDecoder(tinyLimits).push(concat([head, statement, zeroWidthRows(3)])),
    (err) => err.wireCode === "invalid_params" && /row of 0 cells for 1 columns/.test(err.message),
  );
  assert.equal(new CapnpReader(zeroWidthRows(3)).root(1, 1).getStructListLength(0), 3);
  assert.equal(new CapnpReader(zeroWidthRows(3)).root(1, 1).getStructList(0, 0, 0).length, 3);
  // Zero-width elements are charged to the traversal budget by count.
  assert.throws(
    () => new CapnpReader(zeroWidthRows(1000), 16).root(1, 1).getStructListLength(0),
    /traversal budget/,
  );
  assert.throws(
    () => new CapnpReader(zeroWidthRows(1000), 1000).root(1, 1).getStructList(0, 0, 0),
    /traversal budget/,
  );
  // A row of the wrong width for its statement's columns is out of sequence.
  assert.throws(
    () =>
      new ExecuteStreamDecoder(tinyLimits).push(
        concat([head, statement, frame("rows", [{ values: [bad, bad] }])]),
      ),
    (err) => err.wireCode === "invalid_params" && /row of 2 cells for 1 columns/.test(err.message),
  );
  const early = concat([
    frame("head", { operationId: "op-order", statementCount: 1 }),
    frame("rows", [{ values: [bad] }]),
  ]);
  assert.throws(
    () => new ExecuteStreamDecoder(tinyLimits).push(early),
    (err) => err.wireCode === "invalid_params" && /before a statement frame/.test(err.message),
  );
  // The count comes from the list tag alone, and a tag that announces more
  // elements than the list holds is refused.
  assert.equal(
    new CapnpReader(frame("rows", [row(0), row(1), row(2)])).root(1, 1).getStructListLength(0),
    3,
  );
  assert.equal(new CapnpReader(frame("rows", [])).root(1, 1).getStructListLength(0), 0);
  const words = [
    (0n << 32n) | (1n << 48n), // root: struct, 0 data words, 1 pointer
    1n | (7n << 32n) | (1n << 35n), // composite list of one word (its tag)
    (3n << 2n) | (1n << 32n), // tag: 3 elements of one word each
  ];
  const lying = new Uint8Array(8 + words.length * 8);
  const view = new DataView(lying.buffer);
  view.setUint32(4, words.length, true);
  words.forEach((w, i) => view.setBigUint64(8 + i * 8, w, true));
  assert.throws(() => new CapnpReader(lying).root(0, 1).getStructListLength(0), /exceeds payload/);
  assert.throws(
    () =>
      new ExecuteStreamDecoder(tinyLimits).push(
        concat([frame("head", { operationId: "op-order", statementCount: 1 }), lying]),
      ),
    (err) => err.wireCode === "invalid_params" && /does not decode/.test(err.message),
  );
}

// `streamChunks` cancels a body its consumer leaves unread.
{
  let cancelled = false;
  const body = new ReadableStream({
    pull(controller) {
      controller.enqueue(Uint8Array.of(1, 2, 3));
    },
    cancel() {
      cancelled = true;
    },
  });
  for await (const chunk of streamChunks(body)) {
    assert.equal(chunk.byteLength, 3);
    break;
  }
  assert.ok(cancelled, "an early exit cancels the body");
}

// --- Binding hardening -----------------------------------------------------
// A reply for another operation, or with the wrong number of results, is
// refused on both paths.
{
  const other = createDatabaseBinding({
    async execute(req) {
      return { ...inlineReply(req), operationId: "someone-else" };
    },
  });
  await assert.rejects(other.prepare("SELECT 1").all(), (err) => err.code === "invalid_params");
  const short = createDatabaseBinding({
    async execute(req) {
      return { ...inlineReply(req), statements: [] };
    },
  });
  await assert.rejects(short.prepare("SELECT 1").all(), (err) => err.code === "invalid_params");
  const streamedOther = createDatabaseBinding(
    transportOf({ caps: sqliteCaps, body: () => streamOf("someone-else", [row(0)]) }).transport,
  );
  await assert.rejects(
    streamedOther.prepare("SELECT 1").all(),
    (err) => err.code === "invalid_params",
  );
  await assert.rejects(
    (async () => {
      for await (const _ of streamedOther.prepare("SELECT 1").iterate()) {
        // unreachable
      }
    })(),
    (err) => err.code === "invalid_params",
  );
}

// A failed first limits lookup is replaced by a later success, so reads
// then use the session's budget.
{
  let attempts = 0;
  const rows = [row(0), row(1)];
  const transport = {
    async execute(req) {
      return inlineReply(req, rows);
    },
    async limits() {
      attempts += 1;
      if (attempts === 1) {
        throw PluginError.fromWire("unavailable", "limits are down");
      }
      return sqliteCaps;
    },
    async executeStreamed(req) {
      return streamOf(req.operationId, rows.slice(0, 1));
    },
  };
  const db = createDatabaseBinding(transport);
  assert.equal((await db.prepare("SELECT 1").all()).results.length, 2, "inline while unknown");
  assert.deepEqual(await db.limits(), sqliteCaps);
  assert.equal((await db.prepare("SELECT 1").all()).results.length, 1, "streamed once known");
  assert.equal(attempts, 2);
}

// The session's request cap applies locally even without a binding cap, and
// `chunk()` honours explicit zero caps and the tighter of two caps.
{
  const tinyRequest = { ...sqliteCaps, maxRequestBytes: 200 };
  const { transport, calls } = transportOf({
    caps: tinyRequest,
    onInline: (req) => inlineReply(req),
  });
  const db = createDatabaseBinding(transport);
  await assert.rejects(
    db
      .prepare("INSERT INTO t VALUES (?)")
      .bind({ kind: "text", value: "x".repeat(400) })
      .run(),
    (err) =>
      err.code === "limit_exceeded" &&
      err.limit.limit === "maxRequestBytes" &&
      err.limit.cap === 200,
  );
  assert.equal(calls.inline, 0, "refused before it was sent");

  const stmts = Array.from({ length: 5 }, (_, i) =>
    db.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: BigInt(i) }),
  );
  const one = [];
  for await (const batch of db.chunk(stmts, { maxStatements: 0, maxRequestBytes: 0 })) {
    one.push(batch.length);
  }
  assert.deepEqual(one, [5], "explicit zero caps mean one batch");

  const bound = createDatabaseBinding(
    transportOf({ caps: sqliteCaps, onInline: (req) => inlineReply(req) }).transport,
    { maxRequestBytes: 220 },
  );
  const tightened = [];
  for await (const batch of bound.chunk(stmts)) {
    tightened.push(batch.length);
    await bound.batch(batch);
  }
  assert.ok(tightened.length > 1, "the binding's cap tightens the session's");
}

// `iterate()` hands out rows before the body has been read to its end.
{
  const rows = Array.from({ length: 40 }, (_, i) => row(i));
  let pulls = 0;
  const { transport } = transportOf({
    caps: sqliteCaps,
    body: (req) => {
      const bytes = streamOf(req.operationId, rows, { rowsPerFrame: 1 });
      let at = 0;
      return new ReadableStream({
        pull(controller) {
          pulls += 1;
          if (at >= bytes.byteLength) {
            controller.close();
            return;
          }
          controller.enqueue(bytes.slice(at, Math.min(at + 64, bytes.byteLength)));
          at += 64;
        },
      });
    },
  });
  const totalChunks = Math.ceil(streamOf("x", rows, { rowsPerFrame: 1 }).byteLength / 64);
  let first = true;
  for await (const _ of createDatabaseBinding(transport).prepare("SELECT id").iterate()) {
    if (first) {
      assert.ok(pulls < totalChunks / 2, `first row after ${pulls} of ${totalChunks} chunks`);
      first = false;
    }
  }
  assert.equal(first, false, "rows were yielded");
  assert.ok(pulls >= totalChunks);
}

// --- The binding's checks on replies and caps --------------------------
{
  // The tighter of the binding's request cap and the session's applies, as
  // it does to `chunk()`: a request between the two fails locally.
  const { transport, calls } = transportOf({
    caps: { ...sqliteCaps, maxRequestBytes: 200 },
    body: (req) => streamOf(req.operationId, [row(0)]),
  });
  const roomy = createDatabaseBinding(transport, { maxRequestBytes: 1_000_000 });
  await assert.rejects(
    roomy
      .prepare("SELECT ?")
      .bind({ kind: "text", value: "x".repeat(300) })
      .all(),
    (err) => err.wireCode === "limit_exceeded" && err.limit.cap === 200,
  );
  assert.equal(calls.streamed, 0, "refused before it was sent");

  // A result with more rows than its statement asked for is refused, on the
  // inline and the streamed path alike.
  const twoRows = [row(0), row(1)];
  for (const over of [
    transportOf({ onInline: (req) => inlineReply(req, twoRows) }).transport,
    transportOf({ caps: sqliteCaps, body: (req) => streamOf(req.operationId, twoRows) }).transport,
  ]) {
    const db = createDatabaseBinding(over);
    await assert.rejects(
      db.prepare("SELECT id FROM books").first(),
      (err) => err.wireCode === "invalid_params" && /at most 1/.test(err.message),
    );
    assert.equal((await db.prepare("SELECT id FROM books").all()).results.length, 2);
  }

  // A request sent without an operation id takes the one the host minted.
  const minted = createDatabaseBinding(
    transportOf({ onInline: (req) => ({ ...inlineReply(req, [row(0)]), operationId: "minted" }) })
      .transport,
    { operationId: "" },
  );
  assert.equal((await minted.prepare("SELECT 1").all()).results.length, 1);
  const mintedStream = createDatabaseBinding(
    transportOf({ caps: sqliteCaps, body: () => streamOf("minted", [row(0)]) }).transport,
    { operationId: "" },
  );
  assert.equal((await mintedStream.prepare("SELECT 1").all()).results.length, 1);

  // A transport whose `limits()` throws synchronously reads inline, like one
  // whose promise rejects.
  const throwing = transportOf({ onInline: (req) => inlineReply(req, [row(0)]) });
  throwing.transport.limits = () => {
    throw new Error("no limits here");
  };
  assert.equal(
    (await createDatabaseBinding(throwing.transport).prepare("SELECT 1").all()).results.length,
    1,
  );
  assert.equal(throwing.calls.inline, 1);
}

// --- `chunk()` splits a batch to the request caps ------------------------
{
  const { transport } = transportOf({
    caps: { ...sqliteCaps, maxStatements: 2 },
    onInline: (req) => inlineReply(req),
  });
  const db = createDatabaseBinding(transport);
  const stmts = Array.from({ length: 5 }, (_, i) =>
    db.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: BigInt(i) }),
  );
  const batches = [];
  for await (const batch of db.chunk(stmts)) {
    batches.push(batch.length);
    await db.batch(batch);
  }
  assert.deepEqual(batches, [2, 2, 1]);

  // Explicit caps need no session limits.
  const noLimits = createDatabaseBinding({
    async execute(req) {
      return inlineReply(req);
    },
  });
  const explicit = [];
  for await (const batch of noLimits.chunk(stmts, { maxStatements: 3, maxRequestBytes: 0 })) {
    explicit.push(batch.length);
  }
  assert.deepEqual(explicit, [3, 2]);
  await assert.rejects(
    (async () => {
      for await (const _ of noLimits.chunk(stmts)) {
        // unreachable
      }
    })(),
    (err) => err.code === "unsupported",
  );
}

// --- `writeChunks()` resumes at batch boundaries under offset ids --------
{
  const sdk = {
    ...(await import(join(dist, "db-execute.js"))),
    ...(await import(join(dist, "errors.js"))),
  };
  // Pinned in the Rust and Python SDK tests too.
  assert.equal(
    await sdk.writeChunksOperationId("job-1:titles", 2),
    "cb3e8a68-d67e-3739-ee20-30bcaca70026",
  );
  const committed = [];
  let calls = 0;
  let loseReplyAt = 0;
  let failAt = 0;
  const db = createDatabaseBinding({
    async execute(req) {
      calls += 1;
      if (calls === failAt) {
        throw new sdk.PluginError("unavailable", "adapter down");
      }
      // A resent operation id replays its receipt instead of writing again.
      if (!committed.some(([id]) => id === req.operationId)) {
        committed.push([req.operationId, req.statements.length]);
      }
      if (calls === loseReplyAt) {
        throw new sdk.PluginError("unavailable", "reply lost");
      }
      return inlineReply(req);
    },
  });
  const stmts = Array.from({ length: 5 }, (_, i) =>
    db.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: BigInt(i) }),
  );
  const caps = { maxStatements: 2, maxRequestBytes: 0 };

  const paused = await db.writeChunks(stmts, { key: "k", ...caps, stop: (next) => next >= 2 });
  assert.deepEqual(paused, { next: 2, done: false });
  const done = await db.writeChunks(stmts, { key: "k", from: paused.next, ...caps });
  assert.deepEqual(done, { next: 5, done: true });
  assert.deepEqual(
    committed.map(([, n]) => n),
    [2, 2, 1],
    "no statement written twice",
  );
  assert.equal(committed[1][0], await sdk.writeChunksOperationId("k", 2));

  // A lost reply is replayed on resume, not written twice.
  committed.length = 0;
  calls = 0;
  loseReplyAt = 2;
  const lost = await db.writeChunks(stmts, { key: "lost", ...caps }).then(
    () => assert.fail("the second reply is lost"),
    (err) => err,
  );
  assert.ok(lost instanceof sdk.WriteChunksError);
  assert.equal(lost.next, 2);
  assert.equal(lost.code, "unavailable");
  loseReplyAt = 0;
  assert.deepEqual(await db.writeChunks(stmts, { key: "lost", from: lost.next, ...caps }), {
    next: 5,
    done: true,
  });
  assert.equal(committed.length, 3);

  // A failure reports the progress before it; bad input is refused.
  calls = 0;
  failAt = 3;
  await assert.rejects(db.writeChunks(stmts, { key: "fail", ...caps }), (err) => err.next === 4);
  failAt = 0;
  await assert.rejects(
    db.writeChunks(stmts, { key: "", ...caps }),
    (err) => err.code === "invalid_params",
  );
  await assert.rejects(
    db.writeChunks(stmts, { key: "k".repeat(sdk.MAX_WRITE_KEY_BYTES + 1), ...caps }),
    (err) => err.code === "invalid_params",
  );
  await assert.rejects(
    db.writeChunks(stmts, { key: "k", from: 6, ...caps }),
    (err) => err.code === "invalid_params" && err.next === 6,
  );
  assert.deepEqual(await db.writeChunks([], { key: "k", ...caps }), { next: 0, done: true });
}

// `chunkIndexes`: every run encodes within the byte cap, covers each
// statement once and in order, and a lone oversized statement is on its own.
{
  const typed = Array.from({ length: 30 }, (_, i) => ({
    sql: `INSERT INTO t (v) VALUES (?)`,
    parameters: [{ kind: "text", value: "x".repeat((i * 37) % 200) }],
    kind: "execute",
    maxRows: 0,
    resultSelection: "affectedRows",
  }));
  const big = {
    sql: "INSERT INTO t (v) VALUES (?)",
    parameters: [{ kind: "text", value: "y".repeat(2000) }],
    kind: "execute",
    maxRows: 0,
    resultSelection: "affectedRows",
  };
  const all = [...typed.slice(0, 10), big, ...typed.slice(10)];
  const cap = 600;
  const runs = chunkIndexes(all, 0, cap);
  assert.deepEqual(
    runs.flat(),
    all.map((_, i) => i),
  );
  for (const run of runs) {
    const bytes = encodeExecuteRequest({
      operationId: "00000000-0000-4000-8000-000000000000",
      requestHash: "0".repeat(64),
      statements: run.map((i) => all[i]),
      deadlineUnixMs: 0,
    }).byteLength;
    if (run.length > 1) {
      assert.ok(bytes <= cap, `run of ${run.length} encodes to ${bytes} > ${cap}`);
    }
  }
  assert.deepEqual(
    runs.find((run) => run.includes(10)),
    [10],
    "an oversized statement is yielded alone",
  );
  assert.ok(runs.length < all.length, "runs batch more than one statement");
  assert.deepEqual(chunkIndexes([], 5, 100), []);
  assert.deepEqual(chunkIndexes(typed.slice(0, 3), 0, 0), [[0, 1, 2]]);
}

console.log("db-stream ok");
