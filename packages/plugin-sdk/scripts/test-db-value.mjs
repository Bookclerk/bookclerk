import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const dist = join(root, "../dist");

const {
  parseDbValue,
  encodeDbValue,
  decodeDbValue,
} = await import(join(dist, "db-value.js"));
const {
  encodeExecuteRequest,
  decodeExecuteRequest,
  createDatabaseBinding,
  canonicalExecuteRequestHash,
  encodeExecuteResultReply,
  decodeExecuteResultReply,
} = await import(join(dist, "db-execute.js"));
const { guestStatementKind } = await import(join(dist, "guest-sql.js"));

function hex(bytes) {
  return Buffer.from(bytes).toString("hex");
}

const I64_MIN = -0x8000_0000_0000_0000n;
const I64_MAX = 0x7fff_ffff_ffff_ffffn;

const GOLDEN = {
  i64Min:
    "00000000040000000000000002000100000002000000000000000000000000800000000000000000",
  i64Max:
    "000000000400000000000000020001000000020000000000ffffffffffffff7f0000000000000000",
  textB64:
    "0000000006000000000000000200010000000400000000000000000000000000010000004a0000006236343a414141410000000000000000",
  bytes012:
    "0000000005000000000000000200010000000500000000000000000000000000010000001a0000000001020000000000",
  boolTrue:
    "00000000040000000000000002000100010001000000000000000000000000000000000000000000",
  nullBytes:
    "00000000040000000000000002000100050000000000000000000000000000000000000000000000",
  executeRequest:
    "000000001d00000000000000040003000000000000000000000000000000000000000000000000000000000000000000090000001a0000000900000022000000090000001f0000006f70000000000000616263000000000004000000010002000100020001000000050000004a000000090000004f00000053454c454354203f00000000000000000c00000002000100000002000000000000000000000000800000000000000000000004000000000000000000000000000d0000007200000000000500000000000000000000000000090000000a0000006236343a6e6f742d6279746573000000ff00000000000000",
};

const nullBytes = parseDbValue({ kind: "null", value: "bytes" });
assert.deepEqual(nullBytes, { kind: "null", value: "bytes" });

for (const n of [-1, 0, 1]) {
  assert.deepEqual(parseDbValue({ kind: "int64", value: n }), {
    kind: "int64",
    value: BigInt(n),
  });
}

assert.deepEqual(parseDbValue({ kind: "int64", value: I64_MIN }), {
  kind: "int64",
  value: I64_MIN,
});
assert.deepEqual(parseDbValue({ kind: "int64", value: I64_MAX }), {
  kind: "int64",
  value: I64_MAX,
});

const textNul = () => parseDbValue({ kind: "text", value: "héllo\u0000world" });
assert.throws(textNul, /TEXT cannot contain U\+0000/);
assert.throws(
  () => encodeDbValue({ kind: "text", value: "a\u0000b" }),
  /TEXT cannot contain U\+0000/,
);
assert.deepEqual(
  Array.from(decodeDbValue(encodeDbValue({ kind: "bytes", value: Uint8Array.of(0) })).value),
  [0],
);

const multilingual = [
  "汉语",
  "日本語",
  "한국어",
  "𠀀",
  "مرحبا",
  "שלום",
  "नमस्ते",
  "สวัสดี",
  "γειά",
  "привет",
  "🎵🦀",
  "café",
  "cafe\u0301",
];
for (const sample of multilingual) {
  const parsed = parseDbValue({ kind: "text", value: sample });
  assert.equal(parsed.value, sample);
  assert.equal(decodeDbValue(encodeDbValue({ kind: "text", value: sample })).value, sample);
}
assert.notEqual("café", "cafe\u0301");
assert.notEqual(
  hex(encodeDbValue({ kind: "text", value: "café" })),
  hex(encodeDbValue({ kind: "text", value: "cafe\u0301" })),
);

const blob = parseDbValue({ kind: "bytes", value: "b64:AAEC" });
assert.equal(blob.kind, "bytes");
assert.deepEqual(Array.from(blob.value), [0, 1, 2]);

assert.throws(
  () => parseDbValue({ kind: "xml", value: "<a/>" }),
  /unknown DbValue union member/,
);

assert.equal(hex(encodeDbValue({ kind: "int64", value: I64_MIN })), GOLDEN.i64Min);
assert.equal(hex(encodeDbValue({ kind: "int64", value: I64_MAX })), GOLDEN.i64Max);
assert.equal(
  hex(encodeDbValue({ kind: "text", value: "b64:AAAA" })),
  GOLDEN.textB64,
);
assert.equal(
  hex(encodeDbValue({ kind: "bytes", value: Uint8Array.of(0, 1, 2) })),
  GOLDEN.bytes012,
);
assert.equal(hex(encodeDbValue({ kind: "boolean", value: true })), GOLDEN.boolTrue);
assert.equal(
  hex(encodeDbValue({ kind: "null", value: "bytes" })),
  GOLDEN.nullBytes,
);

assert.notEqual(
  hex(encodeDbValue({ kind: "text", value: "b64:AAAA" })),
  hex(encodeDbValue({ kind: "bytes", value: Uint8Array.of(0, 1, 2) })),
);

assert.equal(decodeDbValue(encodeDbValue({ kind: "int64", value: I64_MIN })).value, I64_MIN);
assert.equal(decodeDbValue(encodeDbValue({ kind: "int64", value: I64_MAX })).value, I64_MAX);
assert.equal(decodeDbValue(encodeDbValue({ kind: "text", value: "b64:AAAA" })).value, "b64:AAAA");
assert.deepEqual(
  Array.from(decodeDbValue(encodeDbValue({ kind: "bytes", value: Uint8Array.of(0, 1, 2) })).value),
  [0, 1, 2],
);

const request = {
  operationId: "op",
  requestHash: "abc",
  statements: [
    {
      sql: "SELECT ?",
      parameters: [
        { kind: "int64", value: I64_MIN },
        { kind: "text", value: "b64:not-bytes" },
        { kind: "bytes", value: Uint8Array.of(0xff) },
      ],
      kind: "select",
      maxRows: 1,
      resultSelection: "rows",
    },
  ],
  deadlineUnixMs: 0,
};

const reqBytes = encodeExecuteRequest(request);
assert.equal(hex(reqBytes), GOLDEN.executeRequest);
const back = decodeExecuteRequest(reqBytes);
assert.equal(back.operationId, "op");
assert.equal(back.statements[0].parameters[0].value, I64_MIN);
assert.equal(back.statements[0].parameters[1].value, "b64:not-bytes");
assert.deepEqual(Array.from(back.statements[0].parameters[2].value), [0xff]);

let encodedByBinding = 0;
const binding = createDatabaseBinding(
  {
    async execute(req) {
      encodedByBinding = encodeExecuteRequest(req).byteLength;
      return {
        operationId: req.operationId,
        statements: [],
        timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
      };
    },
  },
  { operationId: "op", requestHash: "abc" },
);
const reply = await binding.execute(request.statements);
assert.equal(reply.operationId, "op");
assert.equal(encodedByBinding, reqBytes.byteLength);

const sequentialIds = [];
const seqBinding = createDatabaseBinding({
  async execute(req) {
    sequentialIds.push(req.operationId);
    assert.notEqual(req.operationId, "op");
    assert.equal(req.requestHash, "");
    return {
      operationId: req.operationId,
      statements: [{ rows: [], columns: [], rowsAffected: 0 }],
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
await seqBinding.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: 1n }).run();
await seqBinding.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: 2n }).run();
assert.equal(sequentialIds.length, 2);
assert.notEqual(sequentialIds[0], sequentialIds[1]);

const GOLDEN_DEADLINE_HASH =
  "648cd28b3223c825c55ea99a7c6e52321ea733656f5965abfe4c7ed4ca21d111";
const deadlineReq = {
  operationId: "op",
  requestHash: "abc",
  statements: [
    {
      sql: "SELECT 1",
      parameters: [],
      kind: "select",
      maxRows: 1,
      resultSelection: "rows",
    },
  ],
  deadlineUnixMs: 0,
};
assert.equal(await canonicalExecuteRequestHash(deadlineReq), GOLDEN_DEADLINE_HASH);
deadlineReq.deadlineUnixMs = 9999999999;
assert.equal(await canonicalExecuteRequestHash(deadlineReq), GOLDEN_DEADLINE_HASH);

const firstSeen = [];
const firstBinding = createDatabaseBinding({
  async execute(req) {
    firstSeen.push({
      maxRows: req.statements[0].maxRows,
      resultSelection: req.statements[0].resultSelection,
    });
    return {
      operationId: req.operationId,
      statements: [
        {
          rows: [
            {
              values: [{ kind: "int64", value: 1n }],
            },
          ],
          columns: [{ name: "n", dbType: "int64" }],
          rowsAffected: 0,
          cursor: "",
        },
      ],
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
const row = await firstBinding.prepare("SELECT n FROM t").first();
assert.equal(firstSeen[0].maxRows, 1);
assert.equal(firstSeen[0].resultSelection, "rows");
assert.equal(row.n.value, 1n);

const col = await firstBinding.prepare("SELECT n FROM t").first("n");
assert.equal(col.value, 1n);

const allResult = await firstBinding.prepare("SELECT n FROM t").all();
assert.equal(allResult.success, true);
assert.ok(Array.isArray(allResult.results));
assert.equal(allResult.results[0].n.value, 1n);
assert.equal(allResult.meta.rows_read, 1);

const runSeen = [];
const runBinding = createDatabaseBinding({
  async execute(req) {
    runSeen.push({
      resultSelection: req.statements[0].resultSelection,
      kind: req.statements[0].kind,
    });
    return {
      operationId: req.operationId,
      statements: [
        {
          rows: [
            {
              values: [{ kind: "int64", value: 2n }],
            },
          ],
          columns: [{ name: "n", dbType: "int64" }],
          rowsAffected: 0,
          cursor: "",
        },
      ],
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
const runResult = await runBinding.prepare("SELECT n FROM t").run();
assert.equal(runSeen[0].resultSelection, "rows");
assert.equal(runSeen[0].kind, "select");
assert.equal(runResult.results[0].n.value, 2n);

const dmlInsertReq = {
  operationId: "",
  requestHash: "",
  deadlineUnixMs: 0,
  statements: [
    {
      sql: "INSERT INTO t VALUES (?)",
      parameters: [{ kind: "int64", value: 1n }],
      kind: guestStatementKind("INSERT INTO t VALUES (?)"),
      maxRows: 0,
      resultSelection: "rows",
    },
  ],
};
assert.equal(dmlInsertReq.statements[0].kind, "execute");
assert.equal(guestStatementKind("INSERT INTO t VALUES (?) RETURNING id"), "returning");
const dmlHash = await canonicalExecuteRequestHash(dmlInsertReq);
assert.equal(dmlHash.length, 64);

const execSeen = [];
const execBinding = createDatabaseBinding({
  async execute(req) {
    execSeen.push(req.statements.length);
    return {
      operationId: req.operationId,
      statements: req.statements.map(() => ({
        rows: [],
        columns: [],
        rowsAffected: 1,
        cursor: "",
      })),
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
const execOut = await execBinding.exec("INSERT INTO t VALUES (1)\nSELECT 1");
assert.equal(execOut.count, 2);
assert.deepEqual(execSeen[0], 2);

const missingColBinding = createDatabaseBinding({
  async execute(req) {
    return {
      operationId: req.operationId,
      statements: [
        {
          rows: [{ values: [{ kind: "int64", value: 1n }] }],
          columns: [{ name: "n", dbType: "int64" }],
          rowsAffected: 0,
          cursor: "",
        },
      ],
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
await assert.rejects(
  () => missingColBinding.prepare("SELECT 1 AS n").first("missing"),
  /column missing not found in first\(\) result/,
);

const mixedSeen = [];
const mixBinding = createDatabaseBinding({
  async execute(req) {
    mixedSeen.push(
      req.statements.map((s) => ({
        selection: s.resultSelection,
        maxRows: s.maxRows,
      })),
    );
    return {
      operationId: req.operationId,
      statements: req.statements.map(() => ({
        rows: [],
        columns: [],
        rowsAffected: 1,
        cursor: "",
      })),
      timing: { attemptElapsedUs: 0, dbExecutionUs: 0, dbTimingSource: "test" },
    };
  },
});
await mixBinding.batch([
  mixBinding.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: 1n }),
  mixBinding.prepare("SELECT n FROM t"),
]);
assert.deepEqual(mixedSeen[0], [
  { selection: "rows", maxRows: 0 },
  { selection: "rows", maxRows: 0 },
]);

await mixBinding.batch([
  mixBinding.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: 1n }).asRun(),
  mixBinding.prepare("SELECT n FROM t").asAll(),
]);
assert.deepEqual(mixedSeen[1], [
  { selection: "affectedRows", maxRows: 0 },
  { selection: "rows", maxRows: 0 },
]);

const batchResults = await mixBinding.batch([
  mixBinding.prepare("INSERT INTO t VALUES (?)").bind({ kind: "int64", value: 1n }),
  mixBinding.prepare("SELECT n FROM t"),
]);
assert.equal(batchResults.length, 2);
assert.equal(batchResults[0].success, true);
assert.equal(batchResults[0].meta.changes, 1);
assert.equal(batchResults[1].success, true);

const i64Reply = {
  operationId: "op",
  statements: [
    {
      rows: [
        {
          values: [
            { kind: "int64", value: I64_MIN },
            { kind: "int64", value: I64_MAX },
            { kind: "bytes", value: Uint8Array.of(0xff) },
            { kind: "text", value: "b64:not-bytes" },
            { kind: "null", value: "int64" },
          ],
        },
      ],
      columns: [
        { name: "lo", dbType: "int64" },
        { name: "hi", dbType: "int64" },
        { name: "blob", dbType: "bytes" },
        { name: "txt", dbType: "text" },
        { name: "n", dbType: "int64" },
      ],
      rowsAffected: 0,
      cursor: "",
    },
  ],
  timing: { attemptElapsedUs: 1, dbExecutionUs: 2, dbTimingSource: "test" },
};
const atomicOk = encodeExecuteResultReply({ ok: i64Reply });
const decodedOk = decodeExecuteResultReply(atomicOk);
assert.equal(decodedOk.statements[0].rows[0].values[0].value, I64_MIN);
assert.equal(decodedOk.statements[0].rows[0].values[1].value, I64_MAX);
assert.deepEqual(
  Array.from(decodedOk.statements[0].rows[0].values[2].value),
  [0xff],
);
assert.equal(decodedOk.statements[0].rows[0].values[3].value, "b64:not-bytes");
assert.equal(decodedOk.statements[0].rows[0].values[4].value, "int64");

const atomicErr = encodeExecuteResultReply({
  err: { code: "unavailable", message: "retry me" },
});
try {
  decodeExecuteResultReply(atomicErr);
  assert.fail("expected PluginError");
} catch (err) {
  assert.equal(err.code, "unavailable");
  assert.equal(err.wireCode, "unavailable");
  assert.equal(err.message, "retry me");
}

assert.throws(() => decodeDbValue(new Uint8Array([0, 0])), /truncated/);
const multi = new Uint8Array(24);
new DataView(multi.buffer).setUint32(0, 1, true);
assert.throws(() => decodeDbValue(multi), /multi-segment/);

assert.equal(typeof readFileSync, "function");
console.log("db-value goldens ok");
