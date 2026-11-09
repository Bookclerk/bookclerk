// Isolate adapter batch runner (`db-adapter.ts`) against a real SQLite
// engine (`node:sqlite`) and the host's receipt-wrap vector
// (`crates/bookclerk-library/fixtures/guest_receipt/isolate_adapter.json`).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");
const fixture = JSON.parse(
  readFileSync(
    join(root, "../../../crates/bookclerk-library/fixtures/guest_receipt/isolate_adapter.json"),
    "utf8",
  ),
);

const {
  GUEST_READ_ONLY_REPLAY_PAYLOAD,
  GUEST_RECEIPT_CLAIM_LOST,
  GUEST_RECEIPT_RESULT_LOST,
  GUEST_REPLAY_PAYLOAD_MAX_BYTES,
  encodeExecuteStream,
  executeAdapterBatch,
  executeAdapterBatchStreamed,
  guestBatchIsReadOnly,
  guestReceiptFinalizeStatements,
  guestReplayPayload,
  runAdapterBatch,
  shouldSkipRemainingGuestWork,
} = await import(join(dist, "db-adapter.js"));
const { decodeExecuteStream } = await import(join(dist, "db-stream.js"));
const { MAX_SCALAR_BYTES } = await import(join(dist, "abi.js"));
const { PluginError } = await import(join(dist, "errors.js"));
const embed = readFileSync(join(root, "../embed/bookclerk_plugin.js"), "utf8");

const OPTIONS = { timingSource: "sqlite_txn" };
// Generous enough for every reply these tests decode.
const LIMITS = {
  maxBytes: 64 << 20,
  bytesLimit: "maxStreamedResultBytes",
  maxRows: 0,
  rowsLimit: "maxStreamedResultRows",
};
const STREAMED = { ...OPTIONS, limits: LIMITS };

// A fixture `DbValue` (serde form) as the SDK's `DbValue`.
function value(raw) {
  switch (raw.kind) {
    case "int64":
      return { kind: "int64", value: BigInt(raw.value) };
    case "bytes":
      return { kind: "bytes", value: Uint8Array.from(raw.value) };
    default:
      return { kind: raw.kind, value: raw.value };
  }
}

// A fixture statement result as the SDK's `StatementResult`.
function result(raw) {
  return {
    rows: raw.rows.map((row) => ({ values: row.values.map(value) })),
    columns: raw.columns,
    rowsAffected: raw.rowsAffected,
  };
}

// A fixture wrapped request as an adapter request (fresh copy).
function request(name, edit = (r) => r) {
  const raw = fixture[name].request;
  return edit({
    operationId: raw.operationId,
    statements: raw.statements.map((s) => ({ ...s, parameters: s.parameters.map(value) })),
    receipt: { ...raw.receipt },
  });
}

// Declared SQLite column type as a `DbType`.
function dbType(declared) {
  switch ((declared ?? "").toUpperCase()) {
    case "INTEGER":
      return "int64";
    case "TEXT":
      return "text";
    case "REAL":
      return "float64";
    case "BLOB":
      return "bytes";
    default:
      return "unspecified";
  }
}

// A `node:sqlite` cell as a `DbValue`.
function cell(v, type) {
  if (v === null) return { kind: "null", value: type };
  if (typeof v === "bigint") return { kind: "int64", value: v };
  if (typeof v === "string") return { kind: "text", value: v };
  if (typeof v === "number") return { kind: "float64", value: v };
  return { kind: "bytes", value: new Uint8Array(v) };
}

// A `DbValue` as a `node:sqlite` bind.
function bind(v) {
  if (v.kind === "null") return null;
  if (v.kind === "boolean") return v.value ? 1n : 0n;
  return v.value;
}

// An isolate adapter's engine: an in-memory SQLite database whose
// transactions lower canonical SQL through the fixture's lowering table.
class Engine {
  constructor() {
    this.db = new DatabaseSync(":memory:");
    for (const ddl of fixture.setup) this.db.exec(ddl);
    this.commits = 0;
    this.rollbacks = 0;
    this.ran = [];
    this.failOn = null;
    this.failRollback = false;
    this.delayMs = 0;
    // Rewrites a query's rows, standing in for an adapter's own result.
    this.mapRows = null;
    // Runs before each statement, standing in for a concurrent attempt.
    this.beforeRun = null;
  }

  // Opens a transaction (the runner's `begin`).
  begin = async () => {
    this.db.exec("BEGIN");
    return {
      run: async (statement) => {
        if (this.delayMs) await new Promise((resolve) => setTimeout(resolve, this.delayMs));
        return this.run(statement);
      },
      commit: async () => {
        this.db.exec("COMMIT");
        this.commits += 1;
      },
      rollback: async () => {
        this.db.exec("ROLLBACK");
        this.rollbacks += 1;
        if (this.failRollback) throw new Error("rollback failed");
      },
    };
  };

  run(statement) {
    const sql = fixture.sqlite[statement.sql];
    assert.ok(sql !== undefined, `no SQLite lowering for ${statement.sql}`);
    this.ran.push(statement.sql);
    if (this.beforeRun) this.beforeRun(statement);
    if (this.failOn && statement.sql.includes(this.failOn))
      throw new Error(`fail on ${this.failOn}`);
    if (statement.kind === "execute") {
      const out = this.db.prepare(sql).run(...statement.parameters.map(bind));
      return { rows: [], columns: [], rowsAffected: Number(out.changes) };
    }
    const prepared = this.db.prepare(sql);
    prepared.setReadBigInts(true);
    const columns = prepared.columns().map((c) => ({ name: c.name, dbType: dbType(c.type) }));
    let rows = prepared.all(...statement.parameters.map(bind)).map((row) => ({
      values: columns.map((c) => cell(row[c.name], c.dbType)),
    }));
    if (this.mapRows) rows = this.mapRows(statement, rows);
    return { rows, columns, rowsAffected: statement.kind === "returning" ? rows.length : 0 };
  }

  receipt(operationId) {
    return this.db
      .prepare(
        "SELECT status, payload, request_hash FROM bookclerk_receipts WHERE operation_id = ?",
      )
      .get(operationId);
  }

  notes() {
    return this.db.prepare("SELECT id, body FROM notes ORDER BY id").all();
  }

  hasNotes() {
    return (
      this.db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'notes'")
        .get() !== undefined
    );
  }
}

async function rejectsWith(promise, code, pattern) {
  await assert.rejects(promise, (err) => {
    assert.ok(err instanceof PluginError, `expected PluginError, got ${err}`);
    assert.equal(err.wireCode ?? err.code, code);
    if (pattern) assert.match(err.message, pattern);
    return true;
  });
}

// The finalize statements and payload are the ones the host derives.
{
  const f = fixture.finalize;
  const partial = {
    operationId: f.partial.operationId,
    statements: f.partial.statements.map(result),
    timing: f.partial.timing,
  };
  const got = guestReceiptFinalizeStatements(partial, f.guestLen, f.guestHash, false);
  assert.equal(got.length, f.statements.length);
  got.forEach((stmt, i) => {
    const want = f.statements[i];
    assert.equal(stmt.sql, want.sql);
    assert.equal(stmt.kind, want.kind);
    assert.equal(stmt.resultSelection, want.resultSelection);
    assert.equal(stmt.maxRows, want.maxRows);
    assert.deepEqual(stmt.parameters, want.parameters.map(value));
  });
  assert.equal(got[0].parameters[0].value, f.statements[0].parameters[0].value, "payload text");
  const readOnly = guestReceiptFinalizeStatements(partial, f.guestLen, f.guestHash, true);
  assert.equal(readOnly[0].parameters[0].value, fixture.readOnlyPayload);
  assert.equal(GUEST_READ_ONLY_REPLAY_PAYLOAD, fixture.readOnlyPayload);
  assert.equal(GUEST_REPLAY_PAYLOAD_MAX_BYTES, fixture.replayPayloadMaxBytes);
}

// Isolates import the helpers from the injected SDK module.
assert.match(embed, /export \{[^}]*\bexecuteAdapterBatch,[^}]*\bexecuteAdapterBatchStreamed\b/s);

// A write batch commits once with its reply recorded on the receipt; a retry
// with the same operationId replays that reply without writing again.
{
  const engine = new Engine();
  const first = await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  assert.equal(engine.commits, 1);
  assert.equal(first.statements.length, 5);
  assert.equal(first.statements[3].rows[0].values[1].value, "first");
  assert.equal(first.timing.dbTimingSource, "sqlite_txn");
  const stored = engine.receipt("fixture-write");
  assert.equal(stored.status, "ok");
  const payload = JSON.parse(stored.payload);
  assert.equal(payload.operation_id, "fixture-write");
  assert.equal(payload.statements.length, 2);
  assert.deepEqual(payload.statements[1].rows[0].values, [
    { kind: "int64", value: 1 },
    { kind: "text", value: "first" },
  ]);

  engine.ran = [];
  const retry = await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  assert.equal(engine.commits, 2);
  // Prune and prior-receipt SELECT only: the guest statements and the stub
  // are padded, and a receipt with a payload needs no finalize.
  assert.equal(engine.ran.length, 2);
  assert.equal(retry.statements.length, 5);
  assert.equal(retry.statements[1].rows[0].values[3].value, stored.payload);
  assert.deepEqual(
    retry.statements.slice(2),
    [0, 0, 0].map(() => ({ rows: [], columns: [], rowsAffected: 0 })),
  );
  assert.equal(engine.notes().length, 1);
}

// The streamed reply decodes to the same reply the inline runner returns.
{
  const engine = new Engine();
  const frames = await executeAdapterBatchStreamed(request("write"), engine.begin, STREAMED);
  const reply = decodeExecuteStream(frames, LIMITS);
  assert.equal(reply.operationId, "fixture-write");
  assert.equal(reply.statements.length, 5);
  assert.equal(reply.statements[3].rows[0].values[1].value, "first");
  assert.equal(engine.commits, 1);
}

// A read-only batch stores the marker, not its rows, and its reads run
// again on retry.
{
  const engine = new Engine();
  await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  assert.equal(guestBatchIsReadOnly(request("readOnly").statements, 1), true);
  const first = await executeAdapterBatch(request("readOnly"), engine.begin, OPTIONS);
  assert.equal(engine.receipt("fixture-read").payload, GUEST_READ_ONLY_REPLAY_PAYLOAD);
  assert.equal(first.statements[2].rows.length, 1);
  engine.db.exec("INSERT INTO notes (id, body) VALUES (2, 'second')");
  const retry = await executeAdapterBatch(request("readOnly"), engine.begin, OPTIONS);
  assert.equal(retry.statements[2].rows.length, 2, "reads ran again");
}

// #761: a streamed read whose frames do not fit one scalar rolls back
// before `limit_exceeded` is answered, so its receipt never commits.
{
  const engine = new Engine();
  await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  const wide = "w".repeat(150 * 1024);
  engine.db.prepare("INSERT INTO notes (id, body) VALUES (?, ?), (?, ?)").run(2n, wide, 3n, wide);
  await rejectsWith(
    executeAdapterBatchStreamed(request("readOnly"), engine.begin, STREAMED),
    "limit_exceeded",
    /maxScalarBytes/,
  );
  assert.equal(engine.rollbacks, 1);
  assert.equal(engine.receipt("fixture-read"), undefined);
  const err = await executeAdapterBatchStreamed(request("readOnly"), engine.begin, STREAMED).catch(
    (e) => e,
  );
  assert.equal(err.details.limit, "maxScalarBytes");
  assert.equal(err.details.cap, MAX_SCALAR_BYTES);
  assert.ok(err.details.used > MAX_SCALAR_BYTES);
}

// #761: a write whose reply cannot be recorded rolls back whole: the guest
// table, its row and the receipt claim all disappear.
{
  const engine = new Engine();
  const oversized = request("write", (r) => {
    r.statements[3].parameters[1] = {
      kind: "text",
      value: "x".repeat(GUEST_REPLAY_PAYLOAD_MAX_BYTES),
    };
    return r;
  });
  await rejectsWith(
    executeAdapterBatchStreamed(oversized, engine.begin, STREAMED),
    "internal",
    /guest replay payload is \d+ bytes; max is 65536/,
  );
  assert.equal(engine.commits, 0);
  assert.equal(engine.rollbacks, 1);
  assert.equal(engine.hasNotes(), false);
  assert.equal(engine.receipt("fixture-write"), undefined);
}

// #761: a write whose frames the host would refuse under the advertised
// streamed budget rolls back before COMMIT; the retry with the same
// operationId commits once.
{
  const engine = new Engine();
  engine.mapRows = (statement, rows) =>
    statement.kind === "returning" ? [...rows, ...rows] : rows;
  const tight = { ...OPTIONS, limits: { ...LIMITS, maxRows: 1 } };
  await rejectsWith(
    executeAdapterBatchStreamed(request("write"), engine.begin, tight),
    "limit_exceeded",
    /maxStreamedResultRows/,
  );
  assert.equal(engine.commits, 0);
  assert.equal(engine.rollbacks, 1);
  assert.equal(engine.hasNotes(), false);
  assert.equal(engine.receipt("fixture-write"), undefined);
  engine.mapRows = null;
  await executeAdapterBatchStreamed(request("write"), engine.begin, tight);
  await executeAdapterBatchStreamed(request("write"), engine.begin, STREAMED);
  assert.equal(engine.commits, 2);
  assert.equal(engine.notes().length, 1, "the write ran once");
}

// #761: a result cell the host would refuse (TEXT with U+0000) rolls the
// batch back before its receipt commits.
{
  const engine = new Engine();
  await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  engine.mapRows = (statement, rows) =>
    statement.sql.startsWith("SELECT id, body FROM notes")
      ? [{ values: [rows[0].values[0], { kind: "text", value: "a\u0000b" }] }]
      : rows;
  await rejectsWith(
    executeAdapterBatch(request("readOnly"), engine.begin, OPTIONS),
    "invalid_params",
    /U\+0000/,
  );
  assert.equal(engine.rollbacks, 1);
  assert.equal(engine.receipt("fixture-read"), undefined);
}

// #761: the receipt records the measured timing a replay answers with.
{
  const engine = new Engine();
  engine.delayMs = 2;
  await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  const timing = JSON.parse(engine.receipt("fixture-write").payload).timing;
  assert.ok(timing.attemptElapsedUs >= 1000, `timing ${JSON.stringify(timing)}`);
  assert.equal(timing.dbExecutionUs, timing.attemptElapsedUs);
}

// #761: another attempt commits the receipt between the prior-receipt
// SELECT and the stub INSERT; the gated write and the stub then claim
// nothing, so the batch rolls back with `unavailable` instead of answering
// zero rows as success.
{
  const engine = new Engine();
  engine.beforeRun = (statement) => {
    if (statement.sql.startsWith("INSERT INTO notes")) {
      engine.db.exec(
        "INSERT INTO bookclerk_receipts (operation_id, operation_kind, request_hash, status, " +
          "payload, created_at, expires_at) VALUES ('fixture-write', 'guest', 'h', 'ok', '{}', 0, 1)",
      );
    }
  };
  await rejectsWith(
    executeAdapterBatch(request("write"), engine.begin, OPTIONS),
    "unavailable",
    new RegExp(GUEST_RECEIPT_CLAIM_LOST),
  );
  assert.equal(engine.commits, 0);
  assert.equal(engine.rollbacks, 1);
}

// A receipt that committed without its reply cannot be replayed.
{
  const engine = new Engine();
  await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  engine.db.exec("UPDATE bookclerk_receipts SET payload = '' WHERE operation_id = 'fixture-write'");
  await rejectsWith(
    executeAdapterBatch(request("write"), engine.begin, OPTIONS),
    "unavailable",
    new RegExp(GUEST_RECEIPT_RESULT_LOST),
  );
  assert.equal(engine.rollbacks, 1);
}

// An in-flight claim with the same hash resumes: the guest work runs again
// (gated writes change nothing) and no finalize stores gated zeros.
{
  const engine = new Engine();
  engine.db
    .prepare(
      "INSERT INTO bookclerk_receipts (operation_id, operation_kind, request_hash, status, payload, created_at, expires_at) VALUES (?, 'guestTyped', ?, 'claimed', '', 'x', '2999')",
    )
    .run("fixture-write", "a".repeat(64));
  engine.ran = [];
  const reply = await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  assert.equal(engine.ran.length, 5, "guest statements and stub ran, no finalize");
  assert.equal(reply.statements[3].rows.length, 0);
  assert.equal(engine.receipt("fixture-write").status, "claimed");
}

// A receipt for a different request skips the guest work (the host reports
// the conflict) and finalizes nothing.
{
  const engine = new Engine();
  engine.db
    .prepare(
      "INSERT INTO bookclerk_receipts (operation_id, operation_kind, request_hash, status, payload, created_at, expires_at) VALUES (?, 'guestTyped', ?, 'ok', '{}', 'x', '2999')",
    )
    .run("fixture-write", "d".repeat(64));
  engine.ran = [];
  const reply = await executeAdapterBatch(request("write"), engine.begin, OPTIONS);
  assert.equal(engine.ran.length, 2);
  assert.equal(reply.statements.length, 5);
  assert.equal(engine.hasNotes(), false);
}

// Without a receipt hint the statements run as given.
{
  const engine = new Engine();
  const bare = {
    operationId: "bare",
    statements: [{ ...request("write").statements[2] }],
    receipt: { guestLen: 0, guestHash: "" },
  };
  const reply = await executeAdapterBatch(bare, engine.begin, OPTIONS);
  assert.equal(reply.statements.length, 1);
  assert.equal(engine.ran.length, 1);
  assert.equal(engine.hasNotes(), true);
}

// A failing statement rolls back, and a failing rollback does not hide it.
{
  const engine = new Engine();
  engine.failOn = "INSERT INTO notes";
  engine.failRollback = true;
  await assert.rejects(
    executeAdapterBatch(request("write"), engine.begin, OPTIONS),
    /fail on INSERT INTO notes/,
  );
  assert.equal(engine.rollbacks, 1);
  assert.equal(engine.commits, 0);
}

// `runAdapterBatch` leaves the commit to the caller.
{
  const engine = new Engine();
  engine.db.exec("BEGIN");
  const reply = await runAdapterBatch(
    request("write"),
    { run: async (s) => engine.run(s) },
    OPTIONS,
  );
  assert.equal(reply.statements.length, 5);
  engine.db.exec("ROLLBACK");
  assert.equal(engine.hasNotes(), false);
}

// Wrap-shape and skip rules.
{
  const partial = (statements) => ({
    operationId: "op",
    statements,
    timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "t" },
  });
  const empty = { rows: [], columns: [], rowsAffected: 0 };
  assert.throws(
    () => guestReceiptFinalizeStatements(partial([empty]), 1, "h", false),
    /wrap returned 1 statements; expected 4/,
  );
  const prior = (hash, status, payload) => ({
    rows: [
      {
        values: [
          { kind: "text", value: "op" },
          { kind: "text", value: hash },
          { kind: "text", value: status },
          { kind: "text", value: payload },
        ],
      },
    ],
    columns: [],
    rowsAffected: 0,
  });
  // Positional fallback when the columns carry no names.
  assert.equal(shouldSkipRemainingGuestWork([empty, prior("h", "ok", "{}")], 4, "h"), true);
  assert.equal(shouldSkipRemainingGuestWork([empty, prior("h", "claimed", "")], 4, "h"), false);
  assert.equal(
    shouldSkipRemainingGuestWork([empty, prior("h", "ok", GUEST_READ_ONLY_REPLAY_PAYLOAD)], 4, "h"),
    false,
  );
  assert.equal(shouldSkipRemainingGuestWork([empty, empty], 4, "h"), false);
  // An empty hash is still a receipt, for another request.
  assert.equal(shouldSkipRemainingGuestWork([empty, prior("", "ok", "")], 4, "h"), true);
  assert.equal(shouldSkipRemainingGuestWork([empty], 4, "h"), false);
  assert.equal(shouldSkipRemainingGuestWork([empty, prior("h", "ok", "{}")], 2, "h"), false);
  assert.equal(guestBatchIsReadOnly([{ kind: "select" }], 1), false, "slice out of range");
  assert.equal(guestBatchIsReadOnly([], 0), false);
  // A non-text hash cell is no receipt.
  const odd = {
    rows: [
      {
        values: [
          { kind: "text", value: "op" },
          { kind: "int64", value: 1n },
        ],
      },
    ],
    columns: [],
    rowsAffected: 0,
  };
  assert.equal(shouldSkipRemainingGuestWork([empty, odd], 4, "h"), false);
  assert.equal(
    guestReceiptFinalizeStatements(
      partial([empty, odd, empty, { ...empty, rowsAffected: 1 }]),
      1,
      "h",
      false,
    ).length,
    1,
  );
}

// Payload encoding refuses what serde cannot decode.
{
  const reply = (values, extra = {}) => ({
    operationId: "op",
    statements: [{ rows: [{ values }], columns: [], rowsAffected: 0, ...extra }],
    timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "t" },
  });
  assert.throws(
    () => guestReplayPayload(reply([{ kind: "float64", value: Number.NaN }])),
    /non-finite/,
  );
  assert.throws(() => guestReplayPayload(reply([], { rowsAffected: -1 })), /not a u64/);
  assert.equal(
    guestReplayPayload(
      reply([
        { kind: "int64", value: 9007199254740993n },
        { kind: "boolean", value: false },
      ]),
    ),
    '{"operation_id":"op","statements":[{"rows":[{"values":[{"kind":"int64","value":9007199254740993},{"kind":"boolean","value":false}]}],"columns":[],"rowsAffected":0}],"timing":{"attemptElapsedUs":0,"dbExecutionUs":0,"dbTimingSource":"t"}}',
  );
}

// Wide rows split into frames that each fit one scalar, or hold one row.
{
  const row = (n) => ({
    values: [
      { kind: "text", value: "r".repeat(n) },
      { kind: "bytes", value: new Uint8Array(8) },
      { kind: "int64", value: 1n },
    ],
  });
  const reply = {
    operationId: "wide",
    statements: [
      {
        rows: [
          row(100 * 1024),
          row(100 * 1024),
          row(300 * 1024),
          ...Array.from({ length: 50 }, () => row(10)),
        ],
        columns: [
          { name: "a", dbType: "text" },
          { name: "b", dbType: "bytes" },
          { name: "c", dbType: "int64" },
        ],
        rowsAffected: 0,
      },
    ],
    timing: { attemptElapsedUs: 1, dbExecutionUs: 1, dbTimingSource: "t" },
  };
  const frames = encodeExecuteStream(reply);
  const decoded = decodeExecuteStream(frames, LIMITS);
  assert.equal(decoded.statements[0].rows.length, 53);
  assert.equal(decoded.statements[0].rows[2].values[0].value.length, 300 * 1024);
  // With no target, one rows frame would pass a scalar and is halved.
  assert.deepEqual(decodeExecuteStream(encodeExecuteStream(reply, 2 ** 40), LIMITS), decoded);
}

console.log("db-adapter ok");
