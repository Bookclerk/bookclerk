/**
 * Universal Cap'n database value domain (`DbValue`) and unpacked codec.
 *
 * Baseline cells are typed null, bool, int64 (`bigint`), finite float64,
 * UTF-8 text (U+0000 forbidden), and bytes (`Uint8Array`, 0x00 allowed). Unknown `kind` values fail closed.
 * JSON `parseDbValue` still accepts `number` / `b64:` strings; the codec
 * always uses the domain types.
 */

import { DB_COLUMN_TYPES, type DbColumnType } from "./abi.js";
import { CapnpMessage, CapnpReader } from "./db-capnp.js";

/** Column / typed-null type tag on the Cap'n `DbValue` wire. */
export type DbType = DbColumnType;

/**
 * Closed Cap'n `DbValue` union.
 *
 * Members are typed null, bool, int64 (`bigint`), finite float64, UTF-8 text
 * (U+0000 forbidden), and bytes (`Uint8Array`, 0x00 allowed). Unknown `kind`
 * values fail closed.
 */
export type DbValue =
  | { kind: "null"; value: DbType }
  | { kind: "boolean"; value: boolean }
  | { kind: "int64"; value: bigint }
  | { kind: "float64"; value: number }
  | { kind: "text"; value: string }
  | { kind: "bytes"; value: Uint8Array };

const KINDS = new Set(["null", "boolean", "int64", "float64", "text", "bytes"]);
const TYPES = new Set<string>(DB_COLUMN_TYPES);

/**
 * Reject U+0000 in BookclerkSQL TEXT (allowed only in BYTES).
 *
 * @param text Candidate TEXT cell.
 * @returns `text` when it contains no NUL.
 * @throws When `text` contains U+0000.
 */
function requirePortableText(text: string): string {
  if (text.includes("\u0000")) {
    throw new Error(
      "BookclerkSQL TEXT cannot contain U+0000 (use BYTES/BLOB for binary)",
    );
  }
  return text;
}

const I64_MIN = -0x8000_0000_0000_0000n;
const I64_MAX = 0x7fff_ffff_ffff_ffffn;

// Ordinal tables come from the generated `abi.ts` projection of
// `schema/plugin.capnp` (index = Cap'n Proto ordinal).
const DB_TYPE_FROM_ORD = DB_COLUMN_TYPES;
const DB_TYPE_ORD: Record<DbType, number> = Object.fromEntries(
  DB_COLUMN_TYPES.map((ty, ord) => [ty, ord]),
) as Record<DbType, number>;

/**
 * Parses a JSON `DbValue`. Unknown union members throw.
 *
 * `int64` accepts a `bigint` or a finite integer `number` (converted to
 * `bigint`). `bytes` accepts a `Uint8Array` or a `b64:` JSON string.
 *
 * @param raw Parsed JSON object.
 * @returns Typed value.
 * @throws When `raw` is not a closed `DbValue`.
 */
export function parseDbValue(raw: unknown): DbValue {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error("DbValue must be an object");
  }
  const obj = raw as { kind?: unknown; value?: unknown };
  if (typeof obj.kind !== "string" || !KINDS.has(obj.kind)) {
    throw new Error(`unknown DbValue union member: ${String(obj.kind)}`);
  }
  switch (obj.kind) {
    case "null":
      if (typeof obj.value !== "string" || !TYPES.has(obj.value)) {
        throw new Error("typed null requires a DbType");
      }
      return { kind: "null", value: obj.value as DbType };
    case "boolean":
      if (typeof obj.value !== "boolean") {
        throw new Error("boolean DbValue requires a boolean");
      }
      return { kind: "boolean", value: obj.value };
    case "int64":
      return { kind: "int64", value: parseInt64(obj.value) };
    case "float64":
      if (typeof obj.value !== "number" || !Number.isFinite(obj.value)) {
        throw new Error("float64 value is not finite");
      }
      return { kind: "float64", value: obj.value };
    case "text":
      if (typeof obj.value !== "string") {
        throw new Error("text DbValue requires a string");
      }
      requirePortableText(obj.value);
      return { kind: "text", value: obj.value };
    case "bytes":
      return { kind: "bytes", value: parseBytes(obj.value) };
    default:
      throw new Error(`unknown DbValue union member: ${obj.kind}`);
  }
}

/**
 * Encodes a standalone unpacked Cap'n `DbValue` message.
 *
 * @param value Domain value (`bigint` / `Uint8Array`, not JSON number / `b64:`).
 * @returns Unpacked Cap'n stream bytes (same encoding as the Rust SDK).
 * @throws When `int64` is out of range, `float64` is not finite, or TEXT
 * contains U+0000.
 */
export function encodeDbValue(value: DbValue): Uint8Array {
  const msg = new CapnpMessage();
  const root = msg.initRoot(2, 1);
  writeDbValue(root, value);
  return msg.finish();
}

/**
 * Decodes a standalone unpacked Cap'n `DbValue` message.
 *
 * @param bytes Unpacked Cap'n stream.
 * @returns Domain value.
 * @throws When the buffer is not a valid `DbValue`.
 */
export function decodeDbValue(bytes: Uint8Array): DbValue {
  const reader = new CapnpReader(bytes);
  return readDbValue(reader.root(2, 1));
}

/**
 * Writes a `DbValue` into an unpacked Cap'n struct (2 data words, 1 pointer).
 *
 * @param root - Cap'n struct builder.
 * @param value - Domain value.
 * @returns Nothing; the struct is mutated in place.
 * @throws When `int64` is out of range, `float64` is not finite, or TEXT
 * contains U+0000.
 */
export function writeDbValue(
  root: { setUint16(i: number, v: number): void; setBool(i: number, v: boolean): void; setInt64(i: number, v: bigint): void; setFloat64(i: number, v: number): void; setText(i: number, v: string): void; setData(i: number, v: Uint8Array): void },
  value: DbValue,
): void {
  switch (value.kind) {
    case "null":
      root.setUint16(0, DB_TYPE_ORD[value.value]);
      root.setUint16(1, 0);
      return;
    case "boolean":
      root.setBool(0, value.value);
      root.setUint16(1, 1);
      return;
    case "int64":
      if (value.value < I64_MIN || value.value > I64_MAX) {
        throw new Error("int64 DbValue is out of range");
      }
      root.setInt64(1, value.value);
      root.setUint16(1, 2);
      return;
    case "float64":
      if (!Number.isFinite(value.value)) {
        throw new Error("float64 value is not finite");
      }
      root.setFloat64(1, value.value);
      root.setUint16(1, 3);
      return;
    case "text":
      requirePortableText(value.value);
      root.setUint16(1, 4);
      root.setText(0, value.value);
      return;
    case "bytes":
      root.setUint16(1, 5);
      root.setData(0, value.value);
      return;
    default: {
      const _exhaustive: never = value;
      throw new Error(`unknown DbValue union member: ${JSON.stringify(_exhaustive)}`);
    }
  }
}

/**
 * Reads a `DbValue` from an unpacked Cap'n struct (2 data words, 1 pointer).
 *
 * @param root - Cap'n struct reader.
 * @returns Domain value.
 * @throws When the union member is unknown or a float64 is not finite.
 */
export function readDbValue(root: {
  getUint16(i: number): number;
  getBool(i: number): boolean;
  getInt64(i: number): bigint;
  getFloat64(i: number): number;
  getText(i: number): string;
  getData(i: number): Uint8Array;
}): DbValue {
  const disc = root.getUint16(1);
  switch (disc) {
    case 0: {
      const ty = DB_TYPE_FROM_ORD[root.getUint16(0)];
      if (ty === undefined) {
        throw new Error("unknown DbType");
      }
      return { kind: "null", value: ty };
    }
    case 1:
      return { kind: "boolean", value: root.getBool(0) };
    case 2:
      return { kind: "int64", value: root.getInt64(1) };
    case 3: {
      const n = root.getFloat64(1);
      if (!Number.isFinite(n)) {
        throw new Error("float64 value is not finite");
      }
      return { kind: "float64", value: n };
    }
    case 4:
      return { kind: "text", value: requirePortableText(root.getText(0)) };
    case 5:
      return { kind: "bytes", value: root.getData(0) };
    default:
      throw new Error(`unknown DbValue union member: ${disc}`);
  }
}

function parseInt64(raw: unknown): bigint {
  let n: bigint;
  if (typeof raw === "bigint") {
    n = raw;
  } else if (typeof raw === "number") {
    if (!Number.isInteger(raw) || !Number.isFinite(raw)) {
      throw new Error("int64 DbValue requires an integer");
    }
    n = BigInt(raw);
  } else if (typeof raw === "string") {
    if (!/^-?\d+$/.test(raw)) {
      throw new Error("int64 DbValue requires an integer");
    }
    n = BigInt(raw);
  } else {
    throw new Error("int64 DbValue requires an integer");
  }
  if (n < I64_MIN || n > I64_MAX) {
    throw new Error("int64 DbValue is out of range");
  }
  return n;
}

function parseBytes(raw: unknown): Uint8Array {
  if (raw instanceof Uint8Array) {
    return raw;
  }
  if (typeof raw !== "string") {
    throw new Error("bytes DbValue requires bytes");
  }
  if (!raw.startsWith("b64:")) {
    throw new Error("bytes DbValue requires bytes");
  }
  return decodeBase64(raw.slice(4));
}

function decodeBase64(b64: string): Uint8Array {
  const bin = globalThis.atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) {
    out[i] = bin.charCodeAt(i);
  }
  return out;
}
