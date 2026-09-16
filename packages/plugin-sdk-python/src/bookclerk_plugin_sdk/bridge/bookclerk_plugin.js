/**
 * GENERATED FILE - do not edit. Bundled from `src/workerd.ts` by
 * `scripts/build-embed.mjs` (`npm run build` in packages/plugin-sdk).
 *
 * Workerd runtime for `@bookclerk/plugin-sdk` / `@bookclerk/plugin-sdk/workerd`
 * (@bookclerk/plugin-sdk@0.1.0). `bookclerk-workerd` injects this module into every
 * plugin isolate under those names; authors import the package, never a
 * relative embed path. Native guests use Rust `serve` / `PluginWorker` instead.
 */

// dist/plugin.js
import { WorkerEntrypoint, RpcTarget } from "cloudflare:workers";

// dist/abi.js
var PRODUCT_API_VERSION = 3;
var MAX_SCALAR_BYTES = 262144;
var MAX_STREAM_WINDOW_BYTES = 1048576;
var MAX_LIST_PAGE = 256;
var MAX_CHECKPOINT_BYTES = 65536;
var MAX_EVENT_PAYLOAD_BYTES = 65536;
var MAX_PLUGIN_MIGRATION_OPS = 256;
var MAX_PLUGIN_MIGRATION_TOTAL_OPS = 2048;
var MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144;
var FEATURE_SCALAR_LIMITS = "rpc.scalarLimits";
var FEATURE_STREAMS = "rpc.streams";
var FEATURE_STORAGE_COPY = "storage.copy";
var PLUGIN_ERROR_CODES = ["invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict"];
var CLI_ARG_KINDS = ["string", "bool", "int", "path"];
var CATALOG_SORTS = ["relevance", "popularity", "rating", "title", "author"];
var CATALOG_FIELDS = ["any", "author", "narrator", "series", "genre"];
var ABRIDGEMENTS = ["unknown", "unabridged", "abridged"];
var DB_TYPES = ["unspecified", "bool", "int64", "float64", "text", "bytes"];
var DB_STATEMENT_KINDS = ["execute", "select", "returning"];
var DB_RESULT_SELECTIONS = ["discard", "affectedRows", "rows"];
var ISOLATION_REQS = ["atomicBatch", "nestedSavepoint", "consistentSnapshot"];
var RESOLVED_SQL_TYPES = ["integer", "real", "text", "blob", "boolean", "null"];
var INTEGER_ARITH_KINDS = ["add", "sub", "mul", "abs"];

// dist/db-capnp.js
var WORD = 8;
var MAX_TRAVERSAL_WORDS = 64 * 1024;
var NO_CAPS = {
  exportCap() {
    throw new Error("message carries a capability but no CapTable was provided");
  },
  importCap(index) {
    if (index === null) {
      return null;
    }
    throw new Error("message carries a capability but no CapTable was provided");
  }
};
var textEncoder = new TextEncoder();
var textDecoder = new TextDecoder();
var CapnpMessage = class {
  buf;
  view;
  /** Words used in the segment, including the root pointer at word 0. */
  usedWords = 1;
  constructor() {
    this.buf = new Uint8Array(256);
    this.view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
  }
  alloc(nWords) {
    const off = this.usedWords;
    this.usedWords += nWords;
    this.ensure((this.usedWords + 1) * WORD);
    return off;
  }
  initRoot(dataWords, pointerWords) {
    const off = this.alloc(dataWords + pointerWords);
    this.writeStructPointer(0, off, dataWords, pointerWords);
    return new CapnpStruct(this, off, dataWords, pointerWords);
  }
  finish() {
    const segBytes = this.usedWords * WORD;
    const out = new Uint8Array(WORD + segBytes);
    const view = new DataView(out.buffer);
    view.setUint32(0, 0, true);
    view.setUint32(4, this.usedWords, true);
    out.set(this.buf.subarray(0, segBytes), WORD);
    return out;
  }
  writeStructPointer(ptrWord, targetWord, dataWords, pointerWords) {
    const offset = targetWord - (ptrWord + 1);
    const word = 0n | BigInt(offset & 1073741823) << 2n | BigInt(dataWords) << 32n | BigInt(pointerWords) << 48n;
    this.setWord(ptrWord, word);
  }
  writeListPointer(ptrWord, targetWord, elementSize, listLength) {
    const offset = targetWord - (ptrWord + 1);
    const word = 1n | BigInt(offset & 1073741823) << 2n | BigInt(elementSize) << 32n | BigInt(listLength) << 35n;
    this.setWord(ptrWord, word);
  }
  writeCapPointer(ptrWord, index) {
    this.setWord(ptrWord, 3n | BigInt(index) << 32n);
  }
  writeEmptyCompositeList(ptrWord, dataWords, pointerWords) {
    const tagWord = this.alloc(1);
    this.writeListPointer(ptrWord, tagWord, 7, 0);
    const tag = 0n | BigInt(dataWords) << 32n | BigInt(pointerWords) << 48n;
    this.setWord(tagWord, tag);
  }
  initStructList(ptrWord, count, dataWords, pointerWords) {
    if (count === 0) {
      this.writeEmptyCompositeList(ptrWord, dataWords, pointerWords);
      return [];
    }
    const elemWords = dataWords + pointerWords;
    const payloadWords = count * elemWords;
    const tagWord = this.alloc(1 + payloadWords);
    this.writeListPointer(ptrWord, tagWord, 7, payloadWords);
    const tag = 0n | BigInt(count) << 2n | BigInt(dataWords) << 32n | BigInt(pointerWords) << 48n;
    this.setWord(tagWord, tag);
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push(new CapnpStruct(this, tagWord + 1 + i * elemWords, dataWords, pointerWords));
    }
    return out;
  }
  setText(ptrWord, value) {
    const encoded = textEncoder.encode(value);
    const withNul = new Uint8Array(encoded.length + 1);
    withNul.set(encoded, 0);
    this.setByteList(ptrWord, withNul);
  }
  setData(ptrWord, value) {
    this.setByteList(ptrWord, value);
  }
  setByteList(ptrWord, bytes) {
    if (bytes.length === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 2, 0);
      return;
    }
    const nWords = Math.ceil(bytes.length / WORD);
    const target = this.alloc(nWords);
    this.buf.set(bytes, target * WORD);
    this.writeListPointer(ptrWord, target, 2, bytes.length);
  }
  /**
   * Allocates a list of `count` pointers.
   *
   * @param ptrWord - Word holding the list pointer.
   * @param count - Number of pointer elements.
   * @returns The word of each element, in order.
   */
  setPointerList(ptrWord, count) {
    if (count === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 6, 0);
      return [];
    }
    const target = this.alloc(count);
    this.writeListPointer(ptrWord, target, 6, count);
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push(target + i);
    }
    return out;
  }
  setTextList(ptrWord, values) {
    const words = this.setPointerList(ptrWord, values.length);
    for (let i = 0; i < values.length; i++) {
      this.setText(words[i], values[i]);
    }
  }
  setDataList(ptrWord, values) {
    const words = this.setPointerList(ptrWord, values.length);
    for (let i = 0; i < values.length; i++) {
      this.setData(words[i], values[i]);
    }
  }
  /**
   * Writes a `List(UInt16)` / `List(UInt32)` (element size codes 3 / 4).
   *
   * @param ptrWord - Word holding the list pointer.
   * @param values - Unsigned integers in range for `byteWidth`.
   * @param byteWidth - `2` for UInt16 (enums), `4` for UInt32.
   */
  setUintList(ptrWord, values, byteWidth) {
    const count = values.length;
    const sizeCode = byteWidth === 2 ? 3 : 4;
    if (count === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, sizeCode, 0);
      return;
    }
    const target = this.alloc(Math.ceil(count * byteWidth / WORD));
    const view = new DataView(this.buf.buffer, this.buf.byteOffset + target * WORD);
    for (let i = 0; i < count; i++) {
      if (byteWidth === 2) {
        view.setUint16(i * 2, values[i], true);
      } else {
        view.setUint32(i * 4, values[i], true);
      }
    }
    this.writeListPointer(ptrWord, target, sizeCode, count);
  }
  setBoolList(ptrWord, values) {
    const count = values.length;
    if (count === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 1, 0);
      return;
    }
    const target = this.alloc(Math.ceil(count / 64));
    const base = target * WORD;
    for (let i = 0; i < count; i++) {
      if (values[i]) {
        this.buf[base + (i >> 3)] |= 1 << (i & 7);
      }
    }
    this.writeListPointer(ptrWord, target, 1, count);
  }
  setUint16(word, fieldIndex, value) {
    this.view.setUint16(word * WORD + fieldIndex * 2, value, true);
  }
  setInt32(word, fieldIndex, value) {
    this.view.setInt32(word * WORD + fieldIndex * 4, value | 0, true);
  }
  setUint32(word, fieldIndex, value) {
    this.view.setUint32(word * WORD + fieldIndex * 4, value >>> 0, true);
  }
  setInt64(word, fieldIndex, value) {
    this.view.setBigInt64(word * WORD + fieldIndex * 8, value, true);
  }
  setUint64(word, fieldIndex, value) {
    this.view.setBigUint64(word * WORD + fieldIndex * 8, value, true);
  }
  setFloat32(word, fieldIndex, value) {
    this.view.setFloat32(word * WORD + fieldIndex * 4, value, true);
  }
  setFloat64(word, fieldIndex, value) {
    this.view.setFloat64(word * WORD + fieldIndex * 8, value, true);
  }
  setBool(word, bitIndex, value) {
    const byteOff = word * WORD + (bitIndex >> 3);
    const mask = 1 << (bitIndex & 7);
    if (value) {
      this.buf[byteOff] |= mask;
    } else {
      this.buf[byteOff] &= ~mask;
    }
  }
  setWord(word, value) {
    this.view.setBigUint64(word * WORD, value, true);
  }
  ensure(bytes) {
    if (this.buf.byteLength >= bytes) {
      return;
    }
    let n = this.buf.byteLength;
    while (n < bytes) {
      n *= 2;
    }
    const next = new Uint8Array(n);
    next.set(this.buf);
    this.buf = next;
    this.view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
  }
};
var CapnpStruct = class _CapnpStruct {
  msg;
  word;
  dataWords;
  pointerWords;
  constructor(msg, word, dataWords, pointerWords) {
    this.msg = msg;
    this.word = word;
    this.dataWords = dataWords;
    this.pointerWords = pointerWords;
  }
  pointerWord(index) {
    if (index >= this.pointerWords) {
      throw new Error("pointer index outside struct pointer section");
    }
    return this.word + this.dataWords + index;
  }
  setUint16(fieldIndex, value) {
    this.msg.setUint16(this.word, fieldIndex, value);
  }
  setInt32(fieldIndex, value) {
    this.msg.setInt32(this.word, fieldIndex, value);
  }
  setUint32(fieldIndex, value) {
    this.msg.setUint32(this.word, fieldIndex, value);
  }
  setInt64(fieldIndex, value) {
    this.msg.setInt64(this.word, fieldIndex, value);
  }
  setUint64(fieldIndex, value) {
    this.msg.setUint64(this.word, fieldIndex, value);
  }
  setFloat32(fieldIndex, value) {
    this.msg.setFloat32(this.word, fieldIndex, value);
  }
  setFloat64(fieldIndex, value) {
    this.msg.setFloat64(this.word, fieldIndex, value);
  }
  setBool(bitIndex, value) {
    this.msg.setBool(this.word, bitIndex, value);
  }
  setText(pointerIndex, value) {
    this.msg.setText(this.pointerWord(pointerIndex), value);
  }
  setData(pointerIndex, value) {
    this.msg.setData(this.pointerWord(pointerIndex), value);
  }
  setCap(pointerIndex, index) {
    this.msg.writeCapPointer(this.pointerWord(pointerIndex), index);
  }
  setTextList(pointerIndex, values) {
    this.msg.setTextList(this.pointerWord(pointerIndex), values);
  }
  setDataList(pointerIndex, values) {
    this.msg.setDataList(this.pointerWord(pointerIndex), values);
  }
  setBoolList(pointerIndex, values) {
    this.msg.setBoolList(this.pointerWord(pointerIndex), values);
  }
  setUint16List(pointerIndex, values) {
    this.msg.setUintList(this.pointerWord(pointerIndex), values, 2);
  }
  setUint32List(pointerIndex, values) {
    this.msg.setUintList(this.pointerWord(pointerIndex), values, 4);
  }
  initStructList(pointerIndex, count, dataWords, pointerWords) {
    return this.msg.initStructList(this.pointerWord(pointerIndex), count, dataWords, pointerWords);
  }
  initStruct(pointerIndex, dataWords, pointerWords) {
    const ptrWord = this.pointerWord(pointerIndex);
    const off = this.msg.alloc(dataWords + pointerWords);
    this.msg.writeStructPointer(ptrWord, off, dataWords, pointerWords);
    return new _CapnpStruct(this.msg, off, dataWords, pointerWords);
  }
};
var CapnpReader = class {
  view;
  segOff;
  size0;
  constructor(bytes) {
    if (bytes.byteLength < WORD) {
      throw new Error("truncated Cap'n message");
    }
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const nsegMinus = this.view.getUint32(0, true);
    if (nsegMinus + 1 !== 1) {
      throw new Error("multi-segment Cap'n messages are not supported");
    }
    const size0 = this.view.getUint32(4, true);
    this.segOff = WORD;
    this.size0 = size0;
    if (this.segOff + size0 * WORD > bytes.byteLength) {
      throw new Error("truncated Cap'n segment");
    }
    if (size0 > MAX_TRAVERSAL_WORDS) {
      throw new Error("Cap'n segment exceeds traversal budget");
    }
  }
  root(dataWords, pointerWords) {
    return this.structAt(0, dataWords, pointerWords);
  }
  structAt(ptrWord, dataWords, pointerWords) {
    void dataWords;
    void pointerWords;
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return new StructReader(this, 0, 0, 0);
    }
    const a = Number(word & 3n);
    if (a === 2 || a === 3) {
      throw new Error("far pointers are not supported");
    }
    if (a !== 0) {
      throw new Error("expected struct pointer");
    }
    const offset = signExtend30(Number(word >> 2n & 0x3fffffffn));
    const dw = Number(word >> 32n & 0xffffn);
    const pw = Number(word >> 48n & 0xffffn);
    const target = ptrWord + 1 + offset;
    this.checkRange(target, dw + pw);
    return new StructReader(this, target, dw, pw);
  }
  readWord(word) {
    this.checkRange(word, 1);
    return this.view.getBigUint64(this.segOff + word * WORD, true);
  }
  checkRange(word, nWords) {
    if (word < 0 || nWords < 0 || word + nWords > this.size0) {
      throw new Error("Cap'n pointer out of segment");
    }
  }
  listPointer(ptrWord, expectSize) {
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return null;
    }
    const a = Number(word & 3n);
    if (a === 2 || a === 3) {
      throw new Error("far pointers are not supported");
    }
    if (a !== 1) {
      throw new Error("expected list pointer");
    }
    const offset = signExtend30(Number(word >> 2n & 0x3fffffffn));
    const c = Number(word >> 32n & 7n);
    const d = Number(word >> 35n);
    if (c !== expectSize) {
      throw new Error(`expected list element size ${expectSize}, found ${c}`);
    }
    return { target: ptrWord + 1 + offset, length: d };
  }
  getUint16(word, fieldIndex) {
    return this.view.getUint16(this.segOff + word * WORD + fieldIndex * 2, true);
  }
  getInt32(word, fieldIndex) {
    return this.view.getInt32(this.segOff + word * WORD + fieldIndex * 4, true);
  }
  getUint32(word, fieldIndex) {
    return this.view.getUint32(this.segOff + word * WORD + fieldIndex * 4, true);
  }
  getInt64(word, fieldIndex) {
    return this.view.getBigInt64(this.segOff + word * WORD + fieldIndex * 8, true);
  }
  getUint64(word, fieldIndex) {
    return this.view.getBigUint64(this.segOff + word * WORD + fieldIndex * 8, true);
  }
  getFloat32(word, fieldIndex) {
    return this.view.getFloat32(this.segOff + word * WORD + fieldIndex * 4, true);
  }
  getFloat64(word, fieldIndex) {
    return this.view.getFloat64(this.segOff + word * WORD + fieldIndex * 8, true);
  }
  getBool(word, bitIndex) {
    const byteOff = this.segOff + word * WORD + (bitIndex >> 3);
    return (this.view.getUint8(byteOff) & 1 << (bitIndex & 7)) !== 0;
  }
  readByteList(ptrWord) {
    const lp = this.listPointer(ptrWord, 2);
    if (lp === null) {
      return new Uint8Array(0);
    }
    const nWords = lp.length === 0 ? 0 : Math.ceil(lp.length / WORD);
    this.checkRange(lp.target, nWords);
    const start = this.segOff + lp.target * WORD;
    if (start + lp.length > this.view.byteLength) {
      throw new Error("truncated Cap'n byte list");
    }
    return new Uint8Array(this.view.buffer, this.view.byteOffset + start, lp.length);
  }
  readText(ptrWord) {
    const bytes = this.readByteList(ptrWord);
    const end = bytes.length > 0 && bytes[bytes.length - 1] === 0 ? bytes.length - 1 : bytes.length;
    return textDecoder.decode(bytes.subarray(0, end));
  }
  readCapIndex(ptrWord) {
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return null;
    }
    if ((word & 3n) !== 3n) {
      throw new Error("expected capability pointer");
    }
    return Number(word >> 32n);
  }
  pointerList(ptrWord) {
    const lp = this.listPointer(ptrWord, 6);
    if (lp === null || lp.length === 0) {
      return [];
    }
    if (lp.length > MAX_TRAVERSAL_WORDS) {
      throw new Error("pointer list exceeds traversal budget");
    }
    this.checkRange(lp.target, lp.length);
    const out = [];
    for (let i = 0; i < lp.length; i++) {
      out.push(lp.target + i);
    }
    return out;
  }
  readTextList(ptrWord) {
    return this.pointerList(ptrWord).map((w) => this.readText(w));
  }
  readDataList(ptrWord) {
    return this.pointerList(ptrWord).map((w) => this.readByteList(w));
  }
  /**
   * Reads a `List(UInt16)` / `List(UInt32)`.
   *
   * @param ptrWord - Word holding the list pointer.
   * @param byteWidth - `2` for UInt16 (enums), `4` for UInt32.
   * @returns Decoded unsigned integers.
   */
  readUintList(ptrWord, byteWidth) {
    const lp = this.listPointer(ptrWord, byteWidth === 2 ? 3 : 4);
    if (lp === null || lp.length === 0) {
      return [];
    }
    this.checkRange(lp.target, Math.ceil(lp.length * byteWidth / WORD));
    const base = this.segOff + lp.target * WORD;
    const out = [];
    for (let i = 0; i < lp.length; i++) {
      out.push(byteWidth === 2 ? this.view.getUint16(base + i * 2, true) : this.view.getUint32(base + i * 4, true));
    }
    return out;
  }
  readBoolList(ptrWord) {
    const lp = this.listPointer(ptrWord, 1);
    if (lp === null || lp.length === 0) {
      return [];
    }
    this.checkRange(lp.target, Math.ceil(lp.length / 64));
    const base = this.segOff + lp.target * WORD;
    const out = [];
    for (let i = 0; i < lp.length; i++) {
      out.push((this.view.getUint8(base + (i >> 3)) >> (i & 7) & 1) === 1);
    }
    return out;
  }
  readStructList(ptrWord, dataWords, pointerWords) {
    void dataWords;
    void pointerWords;
    const lp = this.listPointer(ptrWord, 7);
    if (lp === null || lp.length === 0) {
      return [];
    }
    const tagWord = lp.target;
    this.checkRange(tagWord, 1);
    const tag = this.readWord(tagWord);
    const count = Number(tag >> 2n & 0x3fffffffn);
    const dw = Number(tag >> 32n & 0xffffn);
    const pw = Number(tag >> 48n & 0xffffn);
    const elemWords = dw + pw;
    if (elemWords > 0 && count > Math.floor(lp.length / elemWords)) {
      throw new Error("composite list count exceeds payload");
    }
    if (count > MAX_TRAVERSAL_WORDS) {
      throw new Error("composite list count exceeds traversal budget");
    }
    this.checkRange(tagWord, 1 + count * elemWords);
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push(new StructReader(this, tagWord + 1 + i * elemWords, dw, pw));
    }
    return out;
  }
};
function signExtend30(n) {
  const v = n & 1073741823;
  return v & 536870912 ? v - 1073741824 : v;
}
var StructReader = class _StructReader {
  reader;
  word;
  dataWords;
  pointerWords;
  constructor(reader, word, dataWords, pointerWords) {
    this.reader = reader;
    this.word = word;
    this.dataWords = dataWords;
    this.pointerWords = pointerWords;
  }
  hasData(fieldIndex, size) {
    return (fieldIndex + 1) * size <= this.dataWords * WORD;
  }
  pointerWord(index) {
    if (index >= this.pointerWords) {
      return null;
    }
    return this.word + this.dataWords + index;
  }
  getUint16(fieldIndex) {
    return this.hasData(fieldIndex, 2) ? this.reader.getUint16(this.word, fieldIndex) : 0;
  }
  getInt32(fieldIndex) {
    return this.hasData(fieldIndex, 4) ? this.reader.getInt32(this.word, fieldIndex) : 0;
  }
  getUint32(fieldIndex) {
    return this.hasData(fieldIndex, 4) ? this.reader.getUint32(this.word, fieldIndex) : 0;
  }
  getInt64(fieldIndex) {
    return this.hasData(fieldIndex, 8) ? this.reader.getInt64(this.word, fieldIndex) : 0n;
  }
  getUint64(fieldIndex) {
    return this.hasData(fieldIndex, 8) ? this.reader.getUint64(this.word, fieldIndex) : 0n;
  }
  getFloat32(fieldIndex) {
    return this.hasData(fieldIndex, 4) ? this.reader.getFloat32(this.word, fieldIndex) : 0;
  }
  getFloat64(fieldIndex) {
    return this.hasData(fieldIndex, 8) ? this.reader.getFloat64(this.word, fieldIndex) : 0;
  }
  getBool(bitIndex) {
    if (bitIndex >= this.dataWords * 64) {
      return false;
    }
    return this.reader.getBool(this.word, bitIndex);
  }
  getText(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? "" : this.reader.readText(ptr);
  }
  getData(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? new Uint8Array(0) : this.reader.readByteList(ptr);
  }
  getCapIndex(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? null : this.reader.readCapIndex(ptr);
  }
  getTextList(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readTextList(ptr);
  }
  getDataList(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readDataList(ptr);
  }
  getBoolList(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readBoolList(ptr);
  }
  getUint16List(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readUintList(ptr, 2);
  }
  getUint32List(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readUintList(ptr, 4);
  }
  getStructList(pointerIndex, dataWords, pointerWords) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readStructList(ptr, dataWords, pointerWords);
  }
  getStruct(pointerIndex, dataWords, pointerWords) {
    const ptr = this.pointerWord(pointerIndex);
    if (ptr === null) {
      return new _StructReader(this.reader, 0, 0, 0);
    }
    return this.reader.structAt(ptr, dataWords, pointerWords);
  }
};

// dist/db-value.js
var KINDS = /* @__PURE__ */ new Set(["null", "boolean", "int64", "float64", "text", "bytes"]);
var TYPES = new Set(DB_TYPES);
function requirePortableText(text) {
  if (text.includes("\0")) {
    throw new Error("BookclerkSQL TEXT cannot contain U+0000 (use BYTES/BLOB for binary)");
  }
  return text;
}
var I64_MIN = -0x8000000000000000n;
var I64_MAX = 0x7fffffffffffffffn;
var DB_TYPE_FROM_ORD = DB_TYPES;
var DB_TYPE_ORD = Object.fromEntries(DB_TYPES.map((ty, ord2) => [ty, ord2]));
function parseDbValue(raw) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    throw new Error("DbValue must be an object");
  }
  const obj = raw;
  if (typeof obj.kind !== "string" || !KINDS.has(obj.kind)) {
    throw new Error(`unknown DbValue union member: ${String(obj.kind)}`);
  }
  switch (obj.kind) {
    case "null":
      if (typeof obj.value !== "string" || !TYPES.has(obj.value)) {
        throw new Error("typed null requires a DbType");
      }
      return { kind: "null", value: obj.value };
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
function encodeDbValue(value) {
  const msg = new CapnpMessage();
  const root = msg.initRoot(2, 1);
  writeDbValue(root, value);
  return msg.finish();
}
function decodeDbValue(bytes) {
  const reader = new CapnpReader(bytes);
  return readDbValue(reader.root(2, 1));
}
function writeDbValue(root, value) {
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
      const _exhaustive = value;
      throw new Error(`unknown DbValue union member: ${JSON.stringify(_exhaustive)}`);
    }
  }
}
function readDbValue(root) {
  const disc = root.getUint16(1);
  switch (disc) {
    case 0: {
      const ty = DB_TYPE_FROM_ORD[root.getUint16(0)];
      if (ty === void 0) {
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
function parseInt64(raw) {
  let n;
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
function parseBytes(raw) {
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
function decodeBase64(b64) {
  const bin = globalThis.atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) {
    out[i] = bin.charCodeAt(i);
  }
  return out;
}

// dist/guest-sql.js
function identStart(c) {
  const code = c.charCodeAt(0);
  return code >= 65 && code <= 90 || code >= 97 && code <= 122 || c === "_";
}
function identCont(c) {
  return identStart(c) || c >= "0" && c <= "9";
}
function skipWsComments(sql, start) {
  let i = start;
  while (i < sql.length) {
    while (i < sql.length && /\s/.test(sql[i])) {
      i += 1;
    }
    if (sql[i] === "-" && sql[i + 1] === "-") {
      i += 2;
      while (i < sql.length && sql[i] !== "\n") {
        i += 1;
      }
      continue;
    }
    if (sql[i] === "/" && sql[i + 1] === "*") {
      i += 2;
      while (i + 1 < sql.length && !(sql[i] === "*" && sql[i + 1] === "/")) {
        i += 1;
      }
      i = Math.min(i + 2, sql.length);
      continue;
    }
    break;
  }
  return i;
}
function keywordAt(sql, i, kw) {
  if (i + kw.length > sql.length) {
    return false;
  }
  if (sql.slice(i, i + kw.length).toLowerCase() !== kw.toLowerCase()) {
    return false;
  }
  const beforeOk = i === 0 || !identCont(sql[i - 1]);
  const after = sql[i + kw.length] ?? " ";
  return beforeOk && !identCont(after);
}
function skipIdentOrQuoted(sql, i) {
  const c = sql[i];
  if (c === void 0) {
    return null;
  }
  if (c === '"' || c === "`" || c === "[") {
    const end = c === "[" ? "]" : c;
    let j = i + 1;
    while (j < sql.length) {
      if (sql[j] === end) {
        if (end !== "]" && sql[j + 1] === end) {
          j += 2;
          continue;
        }
        return j + 1;
      }
      j += 1;
    }
    return null;
  }
  if (identStart(c)) {
    let j = i + 1;
    while (j < sql.length && identCont(sql[j])) {
      j += 1;
    }
    return j;
  }
  return null;
}
function skipBalancedParens(sql, start) {
  if (sql[start] !== "(") {
    return null;
  }
  let depth = 0;
  let end = null;
  forEachUnquoted(sql.slice(start), (slice, idx) => {
    if (end !== null) {
      return 1;
    }
    const c = slice[idx];
    if (c === "(") {
      depth += 1;
    } else if (c === ")") {
      depth -= 1;
      if (depth === 0) {
        end = start + idx + 1;
      }
    }
    return 1;
  });
  return end;
}
function sqlAfterLeadingCtes(sql) {
  let i = skipWsComments(sql, 0);
  if (!keywordAt(sql, i, "WITH")) {
    return sql;
  }
  i += 4;
  i = skipWsComments(sql, i);
  if (keywordAt(sql, i, "RECURSIVE")) {
    i += 9;
    i = skipWsComments(sql, i);
  }
  for (; ; ) {
    const next = skipIdentOrQuoted(sql, i);
    if (next === null) {
      return sql;
    }
    i = skipWsComments(sql, next);
    if (sql[i] === "(") {
      const after = skipBalancedParens(sql, i);
      if (after === null) {
        return sql;
      }
      i = skipWsComments(sql, after);
    }
    if (!keywordAt(sql, i, "AS")) {
      return sql;
    }
    i = skipWsComments(sql, i + 2);
    if (keywordAt(sql, i, "NOT")) {
      const afterNot = skipWsComments(sql, i + 3);
      if (keywordAt(sql, afterNot, "MATERIALIZED")) {
        i = skipWsComments(sql, afterNot + 12);
      }
    } else if (keywordAt(sql, i, "MATERIALIZED")) {
      i = skipWsComments(sql, i + 12);
    }
    if (sql[i] !== "(") {
      return sql;
    }
    const afterBody = skipBalancedParens(sql, i);
    if (afterBody === null) {
      return sql;
    }
    i = skipWsComments(sql, afterBody);
    if (sql[i] === ",") {
      i = skipWsComments(sql, i + 1);
      continue;
    }
    return sql.slice(i);
  }
}
function forEachUnquoted(sql, step) {
  let i = 0;
  let inS = false;
  let inD = false;
  let inLine = false;
  let inBlock = false;
  while (i < sql.length) {
    const c = sql[i];
    if (inLine) {
      if (c === "\n") {
        inLine = false;
      }
      i += 1;
      continue;
    }
    if (inBlock) {
      if (c === "*" && sql[i + 1] === "/") {
        inBlock = false;
        i += 2;
        continue;
      }
      i += 1;
      continue;
    }
    if (inS) {
      if (c === "'") {
        if (sql[i + 1] === "'") {
          i += 2;
          continue;
        }
        inS = false;
      }
      i += 1;
      continue;
    }
    if (inD) {
      if (c === '"') {
        if (sql[i + 1] === '"') {
          i += 2;
          continue;
        }
        inD = false;
      }
      i += 1;
      continue;
    }
    if (c === "-" && sql[i + 1] === "-") {
      inLine = true;
      i += 2;
      continue;
    }
    if (c === "/" && sql[i + 1] === "*") {
      inBlock = true;
      i += 2;
      continue;
    }
    if (c === "'") {
      inS = true;
      i += 1;
      continue;
    }
    if (c === '"') {
      inD = true;
      i += 1;
      continue;
    }
    const n = Math.max(1, step(sql, i));
    i += n;
  }
}
function forEachTopLevelKeyword(sql, onKeyword) {
  let depth = 0;
  forEachUnquoted(sql, (slice, idx) => {
    const c = slice[idx];
    if (c === "(") {
      depth += 1;
      return 1;
    }
    if (c === ")") {
      depth = Math.max(0, depth - 1);
      return 1;
    }
    if (depth === 0 && identStart(c)) {
      let j = idx + 1;
      while (j < slice.length && identCont(slice[j])) {
        j += 1;
      }
      onKeyword(idx, slice.slice(idx, j));
      return j - idx;
    }
    return 1;
  });
}
function hasTopLevelKeyword(sql, keyword) {
  const want = keyword.toUpperCase();
  let found = false;
  forEachTopLevelKeyword(sql, (_, kw) => {
    if (kw.toUpperCase() === want) {
      found = true;
    }
  });
  return found;
}
function firstTopLevelKeyword(sql) {
  let first;
  forEachTopLevelKeyword(sql, (_, kw) => {
    if (first === void 0) {
      first = kw.toUpperCase();
    }
  });
  return first;
}
function guestStatementKind(sql) {
  const main = sqlAfterLeadingCtes(sql);
  if (hasTopLevelKeyword(main, "RETURNING")) {
    return "returning";
  }
  const verb = firstTopLevelKeyword(main);
  if (verb === "SELECT" || verb === "VALUES") {
    return "select";
  }
  return "execute";
}
function splitExecQueries(query) {
  return query.split("\n").map((line) => line.trim()).filter((line) => line.length > 0).map((line) => stripTrailingSemicolons(line).trim()).filter((line) => line.length > 0);
}
function stripTrailingSemicolons(line) {
  let end = line.length;
  while (end > 0 && line[end - 1] === ";") {
    end -= 1;
  }
  return line.slice(0, end);
}

// dist/db-execute.js
var KIND_FROM = DB_STATEMENT_KINDS;
var KIND_ORD = Object.fromEntries(DB_STATEMENT_KINDS.map((kind, ord2) => [kind, ord2]));
var SELECT_FROM = DB_RESULT_SELECTIONS;
var SELECT_ORD = Object.fromEntries(DB_RESULT_SELECTIONS.map((sel, ord2) => [sel, ord2]));
var COL_TYPE_FROM = DB_TYPES;
var COL_TYPE_ORD = Object.fromEntries(DB_TYPES.map((ty, ord2) => [ty, ord2]));
function encodeExecuteRequest(request) {
  if (request.statements.length === 0) {
    throw new Error("execute statements must be non-empty");
  }
  const msg = new CapnpMessage();
  const root = msg.initRoot(4, 3);
  root.setText(0, request.operationId);
  root.setText(1, request.requestHash);
  root.setUint64(3, BigInt(request.deadlineUnixMs));
  const stmts = root.initStructList(2, request.statements.length, 1, 2);
  for (let i = 0; i < request.statements.length; i++) {
    writeStatement(stmts[i], request.statements[i]);
  }
  return msg.finish();
}
function decodeExecuteRequest(bytes) {
  const reader = new CapnpReader(bytes);
  const root = reader.root(4, 3);
  const stmtStructs = root.getStructList(2, 1, 2);
  if (stmtStructs.length === 0) {
    throw new Error("execute statements must be non-empty");
  }
  return {
    operationId: root.getText(0),
    requestHash: root.getText(1),
    statements: stmtStructs.map(readStatement),
    deadlineUnixMs: Number(root.getUint64(3))
  };
}
function encodeExecuteResultReply(outcome) {
  const msg = new CapnpMessage();
  const root = msg.initRoot(1, 1);
  if ("ok" in outcome) {
    root.setUint16(0, 0);
    writeExecuteReply(root.initStruct(0, 0, 3), outcome.ok);
  } else {
    root.setUint16(0, 1);
    const err = root.initStruct(0, 0, 2);
    err.setText(0, outcome.err.code);
    err.setText(1, outcome.err.message);
  }
  return msg.finish();
}
function decodeExecuteResultReply(bytes) {
  const root = new CapnpReader(bytes).root(1, 1);
  const disc = root.getUint16(0);
  if (disc === 0) {
    return readExecuteReply(root.getStruct(0, 0, 3));
  }
  if (disc === 1) {
    const err = root.getStruct(0, 0, 2);
    const code = err.getText(0);
    const message = err.getText(1);
    const known = /* @__PURE__ */ new Set([
      "invalid_params",
      "unauthorized",
      "forbidden",
      "not_found",
      "unavailable",
      "unsupported",
      "internal",
      "payload_too_large",
      "deadline_exceeded",
      "invalid_cursor",
      "cancelled",
      "conflict"
    ]);
    throw Object.assign(new Error(message), {
      name: "PluginError",
      code: known.has(code) ? code : "unknown",
      wireCode: code
    });
  }
  throw new Error("unknown ExecuteResultReply union member");
}
function statementResultToD1Result(stmt, timing) {
  const changes = stmt.rowsAffected;
  const durationMs = timing.dbExecutionUs / 1e3;
  const results = stmt.columns.length > 0 ? stmt.rows.map((row) => {
    const obj = {};
    for (let i = 0; i < stmt.columns.length; i++) {
      obj[stmt.columns[i].name] = row.values[i];
    }
    return obj;
  }) : null;
  return {
    success: true,
    results,
    meta: {
      duration: durationMs,
      changes,
      last_row_id: 0,
      changed_db: changes > 0,
      rows_read: stmt.rows.length,
      rows_written: changes
    }
  };
}
function executeReplyToD1Results(reply) {
  return reply.statements.map((stmt) => statementResultToD1Result(stmt, reply.timing));
}
function rowMapFromStatement(result) {
  if (result.rows.length === 0) {
    return null;
  }
  const row = {};
  for (let i = 0; i < result.columns.length; i++) {
    row[result.columns[i].name] = result.rows[0].values[i];
  }
  return row;
}
function columnValueFromRow(row, colName) {
  if (colName in row) {
    return row[colName];
  }
  const lower = colName.toLowerCase();
  for (const [name, value] of Object.entries(row)) {
    if (name.toLowerCase() === lower) {
      return value;
    }
  }
  throw new Error(`column ${colName} not found in first() result`);
}
function writeExecuteReply(root, reply) {
  root.setText(0, reply.operationId);
  const stmts = root.initStructList(1, reply.statements.length, 1, 2);
  for (let i = 0; i < reply.statements.length; i++) {
    writeStatementResult(stmts[i], reply.statements[i]);
  }
  const timing = root.initStruct(2, 2, 1);
  timing.setUint64(0, BigInt(reply.timing.attemptElapsedUs));
  timing.setUint64(1, BigInt(reply.timing.dbExecutionUs));
  timing.setText(0, reply.timing.dbTimingSource);
}
function readExecuteReply(root) {
  return {
    operationId: root.getText(0),
    statements: root.getStructList(1, 1, 2).map(readStatementResult),
    timing: (() => {
      const t = root.getStruct(2, 2, 1);
      return {
        attemptElapsedUs: Number(t.getUint64(0)),
        dbExecutionUs: Number(t.getUint64(1)),
        dbTimingSource: t.getText(0)
      };
    })()
  };
}
function writeStatementResult(s, stmt) {
  s.setUint64(0, BigInt(stmt.rowsAffected));
  const rows = s.initStructList(0, stmt.rows.length, 0, 1);
  for (let i = 0; i < stmt.rows.length; i++) {
    const cells = rows[i].initStructList(0, stmt.rows[i].values.length, 2, 1);
    for (let j = 0; j < stmt.rows[i].values.length; j++) {
      writeDbValue(cells[j], stmt.rows[i].values[j]);
    }
  }
  const cols = s.initStructList(1, stmt.columns.length, 1, 1);
  for (let i = 0; i < stmt.columns.length; i++) {
    cols[i].setText(0, stmt.columns[i].name);
    cols[i].setUint16(0, COL_TYPE_ORD[stmt.columns[i].dbType]);
  }
}
function readStatementResult(s) {
  const columns = s.getStructList(1, 1, 1).map((c) => {
    const ty = COL_TYPE_FROM[c.getUint16(0)];
    if (ty === void 0) {
      throw new Error("unknown DbType");
    }
    return { name: c.getText(0), dbType: ty };
  });
  const rows = s.getStructList(0, 0, 1).map((row) => ({
    values: row.getStructList(0, 2, 1).map(readDbValue)
  }));
  return {
    rows,
    columns,
    rowsAffected: Number(s.getUint64(0))
  };
}
function createDatabaseBinding(transport, options = {}) {
  const runExecute = async (batch, retry) => {
    if (!Array.isArray(batch) || batch.length === 0) {
      throw new Error("execute statements must be non-empty");
    }
    const request = await executeRequestFromBatch(batch, options, retry);
    const encoded = encodeExecuteRequest(request);
    const cap = options.maxRequestBytes ?? 0;
    if (cap > 0 && encoded.byteLength > cap) {
      throw new Error(`atomic request is ${encoded.byteLength} bytes; guest maxRequestBytes is ${cap}`);
    }
    return transport.execute(request);
  };
  const binding = {
    prepare(sql) {
      return makePrepared(binding, sql, [], options.maxResultRows ?? 0, defaultIntent(options));
    },
    batch(statements, opts) {
      const typed = statements.map((s) => s._asTyped());
      return runExecute(typed, opts?.retry).then(executeReplyToD1Results);
    },
    exec(query, opts) {
      const queries = splitExecQueries(query);
      if (queries.length === 0) {
        return Promise.reject(new Error("exec query is empty"));
      }
      const prepared = queries.map((sql) => binding.prepare(sql));
      return binding.batch(prepared, opts).then((results) => ({
        count: results.length,
        duration: results.reduce((sum, r) => sum + r.meta.duration, 0)
      }));
    },
    execute(batch, opts) {
      return runExecute(batch, opts?.retry);
    }
  };
  return binding;
}
function defaultIntent(options) {
  return {
    resultSelection: "rows",
    maxRows: options.maxResultRows ?? 0
  };
}
function makePrepared(binding, sql, parameters, defaultAllRows, intent) {
  const stmt = {
    _intent: intent,
    bind(...values) {
      return makePrepared(binding, sql, values, defaultAllRows, intent);
    },
    asRun() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "affectedRows",
        maxRows: 0
      });
    },
    asFirst() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "rows",
        maxRows: 1
      });
    },
    asAll() {
      return makePrepared(binding, sql, parameters, defaultAllRows, {
        resultSelection: "rows",
        maxRows: defaultAllRows
      });
    },
    run(options) {
      return binding.execute([this.asAll()._asTyped()], options).then((reply) => statementResultToD1Result(reply.statements[0], reply.timing));
    },
    first(colName, options) {
      return binding.execute([this.asFirst()._asTyped()], options).then((reply) => {
        const result = reply.statements[0];
        if (!result) {
          return null;
        }
        const row = rowMapFromStatement(result);
        if (row === null) {
          return null;
        }
        if (colName !== void 0) {
          return columnValueFromRow(row, colName);
        }
        return row;
      });
    },
    raw(options) {
      return binding.execute([this.asAll()._asTyped()], options).then((reply) => {
        const result = reply.statements[0];
        if (!result) {
          return [];
        }
        return result.rows.map((row) => row.values);
      });
    },
    all(options) {
      return binding.execute([this.asAll()._asTyped()], options).then((reply) => statementResultToD1Result(reply.statements[0], reply.timing));
    },
    _asTyped() {
      const used = intent ?? defaultIntent({ maxResultRows: defaultAllRows });
      return {
        sql,
        parameters,
        kind: used.resultSelection === "affectedRows" || used.resultSelection === "discard" ? "execute" : guestStatementKind(sql),
        maxRows: used.maxRows,
        resultSelection: used.resultSelection
      };
    }
  };
  return stmt;
}
async function executeRequestFromBatch(batch, options, retry) {
  const token = retry ?? options.retry;
  return {
    operationId: token?.operationId ?? options.operationId ?? newOperationId(),
    requestHash: token?.requestHash ?? options.requestHash ?? "",
    statements: batch,
    deadlineUnixMs: options.deadlineUnixMs ?? 0
  };
}
async function canonicalExecuteRequestHash(request) {
  const canonical = {
    ...request,
    operationId: "",
    requestHash: "",
    deadlineUnixMs: 0
  };
  const bytes = encodeExecuteRequest(canonical);
  if (globalThis.crypto?.subtle) {
    const digest = await globalThis.crypto.subtle.digest("SHA-256", bytes);
    return hexBytes(new Uint8Array(digest));
  }
  const { createHash } = await import("node:crypto");
  return createHash("sha256").update(bytes).digest("hex");
}
function hexBytes(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}
function newOperationId() {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `op-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}
function writeStatement(s, stmt) {
  s.setText(0, stmt.sql);
  s.setUint16(0, KIND_ORD[stmt.kind]);
  s.setUint16(1, SELECT_ORD[stmt.resultSelection]);
  s.setUint32(1, stmt.maxRows);
  const params = s.initStructList(1, stmt.parameters.length, 2, 1);
  for (let i = 0; i < stmt.parameters.length; i++) {
    writeDbValue(params[i], stmt.parameters[i]);
  }
}
function readStatement(s) {
  const kind = KIND_FROM[s.getUint16(0)];
  const selectionRaw = s.getUint16(1);
  const selection = SELECT_FROM[selectionRaw] ?? "rows";
  if (kind === void 0) {
    throw new Error("unknown DbStatementKind");
  }
  return {
    sql: s.getText(0),
    parameters: s.getStructList(1, 2, 1).map(readDbValue),
    kind,
    maxRows: s.getUint32(1),
    resultSelection: selection
  };
}

// dist/errors.js
var KNOWN_ERROR_CODES = new Set(PLUGIN_ERROR_CODES);
var PluginError = class _PluginError extends Error {
  /** Known `PluginErrorCode` wire string, or `unknown`. */
  code;
  /** Raw wire code, including codes this SDK does not know. */
  wireCode;
  constructor(code, message) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
  }
  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @returns Typed plugin error.
   */
  static fromWire(code, message) {
    return new _PluginError(code, message);
  }
};

// dist/generated-wire.js
function encodeMessage(codec, value, caps = NO_CAPS) {
  const msg = new CapnpMessage();
  codec.write(msg.initRoot(codec.dataWords, codec.pointerCount), value, caps);
  return msg.finish();
}
function decodeMessage(codec, bytes, caps = NO_CAPS) {
  return codec.read(new CapnpReader(bytes).root(codec.dataWords, codec.pointerCount), caps);
}
function ord(table, name, enumName) {
  const i = table.indexOf(name);
  if (i < 0) {
    throw new Error(`unknown ${enumName} value: ${name}`);
  }
  return i;
}
function fromOrd(table, ordinal, enumName) {
  const v = table[ordinal];
  if (v === void 0) {
    throw new Error(`unknown ${enumName} ordinal: ${ordinal}`);
  }
  return v;
}
function unknownUnion(struct, member) {
  return new Error(`unknown ${struct} union member: ${String(member)}`);
}
var EMPTY_BYTES = new Uint8Array(0);
var PluginErrorCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.code ?? "");
    s.setText(1, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      code: s.getText(0),
      message: s.getText(1)
    };
  }
};
var ObjectMetadataCodec = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
    s.setText(1, v.contentType ?? "");
    s.setText(2, v.etag ?? "");
    s.setData(3, v.sha256 ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0)),
      contentType: s.getText(1),
      etag: s.getText(2),
      sha256: s.getData(3)
    };
  }
};
var ObjectInfoCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0))
    };
  }
};
var ListOptionsCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.prefix ?? "");
    s.setText(1, v.cursor ?? "");
    s.setUint32(0, v.limit ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      prefix: s.getText(0),
      cursor: s.getText(1),
      limit: s.getUint32(0)
    };
  }
};
var ListPageCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.objects ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        ObjectInfoCodec.write(items[i], list[i], caps);
      }
    }
    s.setText(1, v.nextCursor ?? "");
  },
  read(s, caps) {
    return {
      objects: s.getStructList(0, 1, 1).map((item) => ObjectInfoCodec.read(item, caps)),
      nextCursor: s.getText(1)
    };
  }
};
var PutResultCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.bytesWritten ?? 0));
    s.setText(1, v.etag ?? "");
    s.setData(2, v.sha256 ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      bytesWritten: Number(s.getUint64(0)),
      etag: s.getText(1),
      sha256: s.getData(2)
    };
  }
};
var CopyResultCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.bytesCopied ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      bytesCopied: Number(s.getUint64(0))
    };
  }
};
var OidcClientTemplateCodec = {
  dataWords: 1,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.clientId ?? "");
    s.setText(1, v.displayName ?? "");
    s.setText(2, v.callbackPath ?? "");
    s.setBool(0, v.publicClient ?? false);
    s.setTextList(3, v.defaultScopes ?? []);
    s.setBool(1, v.issueRefreshToken ?? false);
    s.setText(4, v.originConfigKey ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      clientId: s.getText(0),
      displayName: s.getText(1),
      callbackPath: s.getText(2),
      publicClient: s.getBool(0),
      defaultScopes: s.getTextList(3),
      issueRefreshToken: s.getBool(1),
      originConfigKey: s.getText(4)
    };
  }
};
var OidcClientsOkCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.clients ?? [];
      const items = s.initStructList(0, list.length, 1, 5);
      for (let i = 0; i < items.length; i++) {
        OidcClientTemplateCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      clients: s.getStructList(0, 1, 5).map((item) => OidcClientTemplateCodec.read(item, caps))
    };
  }
};
var OidcClientsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          OidcClientsOkCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("OidcClientsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: OidcClientsOkCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("OidcClientsReply", disc);
    }
  }
};
var ExtensibleConfigCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.schemaVersion ?? 0);
    s.setText(0, v.mediaType ?? "");
    s.setData(1, v.payload ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      schemaVersion: s.getUint32(0),
      mediaType: s.getText(0),
      payload: s.getData(1)
    };
  }
};
var JobInvocationCodec = {
  dataWords: 3,
  pointerCount: 8,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.payloadSchemaVersion ?? 0);
    s.setText(0, v.invocationId ?? "");
    s.setText(1, v.commandType ?? "");
    s.setText(2, v.payloadJson ?? "");
    s.setText(3, v.idempotencyKey ?? "");
    s.setUint32(1, v.attempt ?? 0);
    s.setText(4, v.correlationId ?? "");
    s.setText(5, v.causationId ?? "");
    s.setUint64(1, BigInt(v.deadlineUnixMs ?? 0));
    s.setText(6, v.checkpointJson ?? "");
    s.setUint32(4, v.checkpointSchemaVersion ?? 0);
    s.setUint32(5, v.invocationSequence ?? 0);
    s.setText(7, v.stepId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      payloadSchemaVersion: s.getUint32(0),
      invocationId: s.getText(0),
      commandType: s.getText(1),
      payloadJson: s.getText(2),
      idempotencyKey: s.getText(3),
      attempt: s.getUint32(1),
      correlationId: s.getText(4),
      causationId: s.getText(5),
      deadlineUnixMs: Number(s.getUint64(1)),
      checkpointJson: s.getText(6),
      checkpointSchemaVersion: s.getUint32(4),
      invocationSequence: s.getUint32(5),
      stepId: s.getText(7)
    };
  }
};
var CompletedOutcomeCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
    s.setUint64(0, BigInt(v.bytesCopied ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
      bytesCopied: Number(s.getUint64(0))
    };
  }
};
var RetryableOutcomeCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
    s.setUint64(0, BigInt(v.retryAfterUnixMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0),
      retryAfterUnixMs: Number(s.getUint64(0))
    };
  }
};
var RejectedOutcomeCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0)
    };
  }
};
var CancelledOutcomeCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.message ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      message: s.getText(0)
    };
  }
};
var SuspendedOutcomeCodec = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.checkpointJson ?? "");
    s.setUint32(0, v.checkpointSchemaVersion ?? 0);
    s.setUint64(1, BigInt(v.wakeAtUnixMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      checkpointJson: s.getText(0),
      checkpointSchemaVersion: s.getUint32(0),
      wakeAtUnixMs: Number(s.getUint64(1))
    };
  }
};
var JobOutcomeCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "completed":
        s.setUint16(0, 0);
        if (v.value != null) {
          CompletedOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "retryable":
        s.setUint16(0, 1);
        if (v.value != null) {
          RetryableOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "rejected":
        s.setUint16(0, 2);
        if (v.value != null) {
          RejectedOutcomeCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "cancelled":
        s.setUint16(0, 3);
        if (v.value != null) {
          CancelledOutcomeCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "suspended":
        s.setUint16(0, 4);
        if (v.value != null) {
          SuspendedOutcomeCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("JobOutcome", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "completed", value: CompletedOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "retryable", value: RetryableOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 2:
        return { kind: "rejected", value: RejectedOutcomeCodec.read(s.getStruct(0, 0, 1), caps) };
      case 3:
        return { kind: "cancelled", value: CancelledOutcomeCodec.read(s.getStruct(0, 0, 1), caps) };
      case 4:
        return { kind: "suspended", value: SuspendedOutcomeCodec.read(s.getStruct(0, 2, 1), caps) };
      default:
        throw unknownUnion("JobOutcome", disc);
    }
  }
};
var DomainEventCodec = {
  dataWords: 4,
  pointerCount: 9,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.eventId ?? "");
    s.setText(1, v.eventType ?? "");
    s.setUint32(0, v.schemaVersion ?? 0);
    s.setUint64(1, BigInt(v.occurredAtUnixMs ?? 0));
    s.setText(2, v.accountId ?? "");
    s.setText(3, v.correlationId ?? "");
    s.setText(4, v.causationId ?? "");
    s.setText(5, v.deduplicationKey ?? "");
    s.setUint32(1, v.deliveryAttempt ?? 0);
    s.setData(6, v.payload ?? EMPTY_BYTES);
    s.setText(7, v.checkpointJson ?? "");
    s.setUint32(4, v.checkpointSchemaVersion ?? 0);
    s.setUint32(5, v.invocationSequence ?? 0);
    s.setBool(192, v.resumePending ?? false);
    s.setText(8, v.source ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      eventId: s.getText(0),
      eventType: s.getText(1),
      schemaVersion: s.getUint32(0),
      occurredAtUnixMs: Number(s.getUint64(1)),
      accountId: s.getText(2),
      correlationId: s.getText(3),
      causationId: s.getText(4),
      deduplicationKey: s.getText(5),
      deliveryAttempt: s.getUint32(1),
      payload: s.getData(6),
      checkpointJson: s.getText(7),
      checkpointSchemaVersion: s.getUint32(4),
      invocationSequence: s.getUint32(5),
      resumePending: s.getBool(192),
      source: s.getText(8)
    };
  }
};
var EventAckCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    return {
      dummy: void 0
    };
  }
};
var EventRetryCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.retryAtUnixMs ?? 0));
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      retryAtUnixMs: Number(s.getUint64(0)),
      reason: s.getText(0)
    };
  }
};
var EventRejectCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      reason: s.getText(0)
    };
  }
};
var EventDeadLetterCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      reason: s.getText(0)
    };
  }
};
var EventSuspendedCodec = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.checkpointJson ?? "");
    s.setUint32(0, v.checkpointSchemaVersion ?? 0);
    s.setUint64(1, BigInt(v.wakeAtUnixMs ?? 0));
    s.setText(1, v.wakeOnEventType ?? "");
    s.setText(2, v.wakeOnFilterJson ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      checkpointJson: s.getText(0),
      checkpointSchemaVersion: s.getUint32(0),
      wakeAtUnixMs: Number(s.getUint64(1)),
      wakeOnEventType: s.getText(1),
      wakeOnFilterJson: s.getText(2)
    };
  }
};
var EventResultCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ack":
        s.setUint16(0, 0);
        if (v.value != null) {
          EventAckCodec.write(s.initStruct(0, 0, 0), v.value, caps);
        }
        break;
      case "retry":
        s.setUint16(0, 1);
        if (v.value != null) {
          EventRetryCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "reject":
        s.setUint16(0, 2);
        if (v.value != null) {
          EventRejectCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "deadLetter":
        s.setUint16(0, 3);
        if (v.value != null) {
          EventDeadLetterCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "suspended":
        s.setUint16(0, 4);
        if (v.value != null) {
          EventSuspendedCodec.write(s.initStruct(0, 2, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventResult", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ack", value: EventAckCodec.read(s.getStruct(0, 0, 0), caps) };
      case 1:
        return { kind: "retry", value: EventRetryCodec.read(s.getStruct(0, 1, 1), caps) };
      case 2:
        return { kind: "reject", value: EventRejectCodec.read(s.getStruct(0, 0, 1), caps) };
      case 3:
        return { kind: "deadLetter", value: EventDeadLetterCodec.read(s.getStruct(0, 0, 1), caps) };
      case 4:
        return { kind: "suspended", value: EventSuspendedCodec.read(s.getStruct(0, 2, 3), caps) };
      default:
        throw unknownUnion("EventResult", disc);
    }
  }
};
var EventBatchCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.events ?? [];
      const items = s.initStructList(0, list.length, 4, 9);
      for (let i = 0; i < items.length; i++) {
        DomainEventCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      events: s.getStructList(0, 4, 9).map((item) => DomainEventCodec.read(item, caps))
    };
  }
};
var EventBatchReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 1, 1);
          for (let i = 0; i < items.length; i++) {
            EventResultCodec.write(items[i], list[i], caps);
          }
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventBatchReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getStructList(0, 1, 1).map((item) => EventResultCodec.read(item, caps)) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EventBatchReply", disc);
    }
  }
};
var JobControllerCodec = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    if (v.invocation != null) {
      JobInvocationCodec.write(s.initStruct(0, 3, 8), v.invocation, caps);
    }
    s.setCap(1, caps.exportCap(v.input));
    s.setCap(2, caps.exportCap(v.output));
    s.setCap(3, caps.exportCap(v.progress));
    s.setCap(4, caps.exportCap(v.cancel));
  },
  read(s, caps) {
    return {
      invocation: JobInvocationCodec.read(s.getStruct(0, 3, 8), caps),
      input: caps.importCap(s.getCapIndex(1)),
      output: caps.importCap(s.getCapIndex(2)),
      progress: caps.importCap(s.getCapIndex(3)),
      cancel: caps.importCap(s.getCapIndex(4))
    };
  }
};
var HeadOkCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.meta != null) {
      ObjectMetadataCodec.write(s.initStruct(0, 1, 4), v.meta, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      meta: ObjectMetadataCodec.read(s.getStruct(0, 1, 4), caps)
    };
  }
};
var HeadReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          HeadOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HeadReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: HeadOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HeadReply", disc);
    }
  }
};
var ListReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ListPageCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ListReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ListPageCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ListReply", disc);
    }
  }
};
var PutReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PutResultCodec.write(s.initStruct(0, 1, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PutReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PutResultCodec.read(s.getStruct(0, 1, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PutReply", disc);
    }
  }
};
var CopyReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CopyResultCodec.write(s.initStruct(0, 1, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CopyReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CopyResultCodec.read(s.getStruct(0, 1, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CopyReply", disc);
    }
  }
};
var EmptyReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EmptyReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok" };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EmptyReply", disc);
    }
  }
};
var HandleReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          JobOutcomeCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HandleReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: JobOutcomeCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HandleReply", disc);
    }
  }
};
var HealthOkCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.ok ?? false);
    s.setText(0, v.detail ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      ok: s.getBool(0),
      detail: s.getText(0)
    };
  }
};
var HealthReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          HealthOkCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HealthReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: HealthOkCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("HealthReply", disc);
    }
  }
};
var AdapterSessionReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        s.setCap(0, caps.exportCap(v.value));
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("AdapterSessionReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: caps.importCap(s.getCapIndex(0)) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("AdapterSessionReply", disc);
    }
  }
};
var CliSchemaCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.commands ?? [];
      const items = s.initStructList(0, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        CliCommandSpecCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      commands: s.getStructList(0, 0, 3).map((item) => CliCommandSpecCodec.read(item, caps))
    };
  }
};
var CliCommandSpecCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.name ?? "");
    if (v.about !== void 0) {
      s.setText(1, v.about ?? "");
    }
    {
      const list = v.args ?? [];
      const items = s.initStructList(2, list.length, 1, 5);
      for (let i = 0; i < items.length; i++) {
        CliArgSpecCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    const out = {
      name: s.getText(0),
      args: s.getStructList(2, 1, 5).map((item) => CliArgSpecCodec.read(item, caps))
    };
    const aboutValue = s.getText(1);
    if (!(aboutValue === "")) {
      out.about = aboutValue;
    }
    return out;
  }
};
var CliArgSpecCodec = {
  dataWords: 1,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    if (v.long !== void 0) {
      s.setText(1, v.long ?? "");
    }
    if (v.short !== void 0) {
      s.setText(2, v.short ?? "");
    }
    s.setUint16(0, ord(CLI_ARG_KINDS, v.kind ?? CLI_ARG_KINDS[0], "CliArgKind"));
    s.setBool(16, v.required ?? false);
    if (v.default !== void 0) {
      s.setText(3, v.default ?? "");
    }
    if (v.about !== void 0) {
      s.setText(4, v.about ?? "");
    }
    s.setBool(17, v.positional ?? false);
  },
  read(s, caps) {
    void caps;
    const out = {
      name: s.getText(0),
      kind: fromOrd(CLI_ARG_KINDS, s.getUint16(0), "CliArgKind"),
      required: s.getBool(16),
      positional: s.getBool(17)
    };
    const longValue = s.getText(1);
    if (!(longValue === "")) {
      out.long = longValue;
    }
    const shortValue = s.getText(2);
    if (!(shortValue === "")) {
      out.short = shortValue;
    }
    const defaultValue = s.getText(3);
    if (!(defaultValue === "")) {
      out.default = defaultValue;
    }
    const aboutValue = s.getText(4);
    if (!(aboutValue === "")) {
      out.about = aboutValue;
    }
    return out;
  }
};
var CliArgCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setText(1, v.value ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      value: s.getText(1)
    };
  }
};
var CliInvokeParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.command ?? "");
    {
      const list = v.args ?? [];
      const items = s.initStructList(1, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        CliArgCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      command: s.getText(0),
      args: s.getStructList(1, 0, 2).map((item) => CliArgCodec.read(item, caps))
    };
  }
};
var CliInvokeResultCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setInt32(0, v.exitCode ?? 0);
    s.setText(0, v.stdout ?? "");
    s.setText(1, v.stderr ?? "");
    if (v.payload != null) {
      ExtensibleConfigCodec.write(s.initStruct(2, 1, 2), v.payload, caps);
    }
  },
  read(s, caps) {
    return {
      exitCode: s.getInt32(0),
      stdout: s.getText(0),
      stderr: s.getText(1),
      payload: ExtensibleConfigCodec.read(s.getStruct(2, 1, 2), caps)
    };
  }
};
var CliSchemaReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CliSchemaCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CliSchemaReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CliSchemaCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CliSchemaReply", disc);
    }
  }
};
var CliInvokeReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CliInvokeResultCodec.write(s.initStruct(0, 1, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CliInvokeReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CliInvokeResultCodec.read(s.getStruct(0, 1, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CliInvokeReply", disc);
    }
  }
};
var DiagnoseResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.lines ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      lines: s.getTextList(0)
    };
  }
};
var DiagnoseReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DiagnoseResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DiagnoseReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DiagnoseResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DiagnoseReply", disc);
    }
  }
};
var SourceAccountCodec = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setText(1, v.source ?? "");
    s.setText(2, v.marketplace ?? "");
    if (v.label !== void 0) {
      s.setText(3, v.label ?? "");
    }
    s.setBool(0, v.scanEnabled ?? false);
  },
  read(s, caps) {
    void caps;
    const out = {
      accountId: s.getText(0),
      source: s.getText(1),
      marketplace: s.getText(2),
      scanEnabled: s.getBool(0)
    };
    const labelValue = s.getText(3);
    if (!(labelValue === "")) {
      out.label = labelValue;
    }
    return out;
  }
};
var LoginParamsCodec = {
  dataWords: 2,
  pointerCount: 10,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setText(1, v.marketplace ?? "");
    if (v.label !== void 0) {
      s.setText(2, v.label ?? "");
    }
    if (v.email !== void 0) {
      s.setText(3, v.email ?? "");
    }
    if (v.password !== void 0) {
      s.setText(4, v.password ?? "");
    }
    s.setBool(0, v.force ?? false);
    if (v.callbackBind !== void 0) {
      s.setText(5, v.callbackBind ?? "");
    }
    if (v.callbackIpc !== void 0) {
      s.setText(6, v.callbackIpc ?? "");
    }
    if (v.callbackPublicBase !== void 0) {
      s.setText(7, v.callbackPublicBase ?? "");
    }
    s.setBool(1, v.external ?? false);
    if (v.responseUrl !== void 0) {
      s.setText(8, v.responseUrl ?? "");
    }
    s.setBool(2, v.showQr ?? false);
    if (v.timeoutSecs !== void 0) {
      s.setUint64(1, BigInt(v.timeoutSecs ?? 0));
    }
    if (v.extra != null) {
      ExtensibleConfigCodec.write(s.initStruct(9, 1, 2), v.extra, caps);
    }
  },
  read(s, caps) {
    const out = {
      pluginDataDir: s.getText(0),
      marketplace: s.getText(1),
      force: s.getBool(0),
      external: s.getBool(1),
      showQr: s.getBool(2),
      extra: ExtensibleConfigCodec.read(s.getStruct(9, 1, 2), caps)
    };
    const labelValue = s.getText(2);
    if (!(labelValue === "")) {
      out.label = labelValue;
    }
    const emailValue = s.getText(3);
    if (!(emailValue === "")) {
      out.email = emailValue;
    }
    const passwordValue = s.getText(4);
    if (!(passwordValue === "")) {
      out.password = passwordValue;
    }
    const callbackBindValue = s.getText(5);
    if (!(callbackBindValue === "")) {
      out.callbackBind = callbackBindValue;
    }
    const callbackIpcValue = s.getText(6);
    if (!(callbackIpcValue === "")) {
      out.callbackIpc = callbackIpcValue;
    }
    const callbackPublicBaseValue = s.getText(7);
    if (!(callbackPublicBaseValue === "")) {
      out.callbackPublicBase = callbackPublicBaseValue;
    }
    const responseUrlValue = s.getText(8);
    if (!(responseUrlValue === "")) {
      out.responseUrl = responseUrlValue;
    }
    const timeoutSecsValue = Number(s.getUint64(1));
    if (!(timeoutSecsValue === 0)) {
      out.timeoutSecs = timeoutSecsValue;
    }
    return out;
  }
};
var LoginResultCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.account != null) {
      SourceAccountCodec.write(s.initStruct(0, 1, 4), v.account, caps);
    }
    if (v.credentials !== void 0) {
      s.setData(1, v.credentials ?? EMPTY_BYTES);
    }
  },
  read(s, caps) {
    const out = {
      account: SourceAccountCodec.read(s.getStruct(0, 1, 4), caps)
    };
    const credentialsValue = s.getData(1);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  }
};
var LoginStartResultCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.sessionId ?? "");
    s.setText(1, v.url ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      sessionId: s.getText(0),
      url: s.getText(1)
    };
  }
};
var LoginCompleteParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.sessionId ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      sessionId: s.getText(0)
    };
  }
};
var LoginReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          LoginResultCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("LoginReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: LoginResultCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("LoginReply", disc);
    }
  }
};
var LoginStartReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          LoginStartResultCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("LoginStartReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: LoginStartResultCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("LoginStartReply", disc);
    }
  }
};
var AccountCredentialCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setData(1, v.credentials ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      accountId: s.getText(0),
      credentials: s.getData(1)
    };
  }
};
var ScanParamsCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setTextList(1, v.accounts ?? []);
    s.setUint32(0, v.pageSize ?? 0);
    s.setBool(32, v.importEpisodes ?? false);
    s.setBool(33, v.importPlusTitles ?? false);
    {
      const list = v.credentials ?? [];
      const items = s.initStructList(2, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        AccountCredentialCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      pluginDataDir: s.getText(0),
      accounts: s.getTextList(1),
      pageSize: s.getUint32(0),
      importEpisodes: s.getBool(32),
      importPlusTitles: s.getBool(33),
      credentials: s.getStructList(2, 0, 2).map((item) => AccountCredentialCodec.read(item, caps))
    };
  }
};
var ScanBookCodec = {
  dataWords: 1,
  pointerCount: 13,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    s.setText(1, v.productId ?? "");
    s.setText(2, v.title ?? "");
    if (v.marketplace !== void 0) {
      s.setText(3, v.marketplace ?? "");
    }
    if (v.asin !== void 0) {
      s.setText(4, v.asin ?? "");
    }
    if (v.isbn !== void 0) {
      s.setText(5, v.isbn ?? "");
    }
    if (v.authors !== void 0) {
      s.setText(6, v.authors ?? "");
    }
    if (v.narrators !== void 0) {
      s.setText(7, v.narrators ?? "");
    }
    if (v.series !== void 0) {
      s.setText(8, v.series ?? "");
    }
    if (v.seriesIndex !== void 0) {
      s.setText(9, v.seriesIndex ?? "");
    }
    if (v.contentKind !== void 0) {
      s.setText(10, v.contentKind ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(11, v.publisher ?? "");
    }
    if (v.lengthMinutes !== void 0) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.subtitle !== void 0) {
      s.setText(12, v.subtitle ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      accountId: s.getText(0),
      productId: s.getText(1),
      title: s.getText(2)
    };
    const marketplaceValue = s.getText(3);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    const asinValue = s.getText(4);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(5);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const authorsValue = s.getText(6);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(7);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(8);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesIndexValue = s.getText(9);
    if (!(seriesIndexValue === "")) {
      out.seriesIndex = seriesIndexValue;
    }
    const contentKindValue = s.getText(10);
    if (!(contentKindValue === "")) {
      out.contentKind = contentKindValue;
    }
    const publisherValue = s.getText(11);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const subtitleValue = s.getText(12);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    return out;
  }
};
var ScanSummaryCodec = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    s.setUint32(0, v.accounts ?? 0);
    s.setUint32(1, v.booksUpserted ?? 0);
    s.setUint32(2, v.pages ?? 0);
    s.setUint32(3, v.skippedDisabled ?? 0);
    {
      const list = v.books ?? [];
      const items = s.initStructList(0, list.length, 1, 13);
      for (let i = 0; i < items.length; i++) {
        ScanBookCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      accounts: s.getUint32(0),
      booksUpserted: s.getUint32(1),
      pages: s.getUint32(2),
      skippedDisabled: s.getUint32(3),
      books: s.getStructList(0, 1, 13).map((item) => ScanBookCodec.read(item, caps))
    };
  }
};
var ScanReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ScanSummaryCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ScanReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ScanSummaryCodec.read(s.getStruct(0, 2, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ScanReply", disc);
    }
  }
};
var FetchOptionsCodec = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.widevine ?? false);
    s.setBool(1, v.xheAac ?? false);
    if (v.widevineCdmPath !== void 0) {
      s.setText(0, v.widevineCdmPath ?? "");
    }
    if (v.widevineCdmProvider !== void 0) {
      s.setText(1, v.widevineCdmProvider ?? "");
    }
    s.setBool(2, v.downloadCover ?? false);
    s.setBool(3, v.downloadPdf ?? false);
    s.setText(2, v.coverSize ?? "");
    s.setText(3, v.chapterLayout ?? "");
    s.setBool(4, v.stripAudibleBrandAudio ?? false);
    s.setBool(5, v.downloadClipsBookmarks ?? false);
    s.setBool(6, v.retainAaxFile ?? false);
    s.setUint32(1, v.downloadSpeedLimitKbps ?? 0);
    s.setBool(7, v.saveMetadataJson ?? false);
  },
  read(s, caps) {
    void caps;
    const out = {
      widevine: s.getBool(0),
      xheAac: s.getBool(1),
      downloadCover: s.getBool(2),
      downloadPdf: s.getBool(3),
      coverSize: s.getText(2),
      chapterLayout: s.getText(3),
      stripAudibleBrandAudio: s.getBool(4),
      downloadClipsBookmarks: s.getBool(5),
      retainAaxFile: s.getBool(6),
      downloadSpeedLimitKbps: s.getUint32(1),
      saveMetadataJson: s.getBool(7)
    };
    const widevineCdmPathValue = s.getText(0);
    if (!(widevineCdmPathValue === "")) {
      out.widevineCdmPath = widevineCdmPathValue;
    }
    const widevineCdmProviderValue = s.getText(1);
    if (!(widevineCdmProviderValue === "")) {
      out.widevineCdmProvider = widevineCdmProviderValue;
    }
    return out;
  }
};
var FetchTitleParamsCodec = {
  dataWords: 0,
  pointerCount: 7,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setText(1, v.accountId ?? "");
    s.setText(2, v.titleId ?? "");
    s.setText(3, v.cacheDir ?? "");
    if (v.credentials !== void 0) {
      s.setData(4, v.credentials ?? EMPTY_BYTES);
    }
    if (v.sourceConfig != null) {
      ExtensibleConfigCodec.write(s.initStruct(5, 1, 2), v.sourceConfig, caps);
    }
    if (v.fetch != null) {
      FetchOptionsCodec.write(s.initStruct(6, 1, 4), v.fetch, caps);
    }
  },
  read(s, caps) {
    const out = {
      pluginDataDir: s.getText(0),
      accountId: s.getText(1),
      titleId: s.getText(2),
      cacheDir: s.getText(3),
      sourceConfig: ExtensibleConfigCodec.read(s.getStruct(5, 1, 2), caps),
      fetch: FetchOptionsCodec.read(s.getStruct(6, 1, 4), caps)
    };
    const credentialsValue = s.getData(4);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  }
};
var PlainPartCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.path ?? "");
    if (v.title !== void 0) {
      s.setText(1, v.title ?? "");
    }
    if (v.durationMs !== void 0) {
      s.setUint64(0, BigInt(v.durationMs ?? 0));
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      path: s.getText(0)
    };
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const durationMsValue = Number(s.getUint64(0));
    if (!(durationMsValue === 0)) {
      out.durationMs = durationMsValue;
    }
    return out;
  }
};
var ChapterMarkerCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.title ?? "");
    s.setUint64(0, BigInt(v.startMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      title: s.getText(0),
      startMs: Number(s.getUint64(0))
    };
  }
};
var PlainFetchCodec = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    {
      const list = v.parts ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        PlainPartCodec.write(items[i], list[i], caps);
      }
    }
    if (v.m4bPath !== void 0) {
      s.setText(1, v.m4bPath ?? "");
    }
    if (v.coverPath !== void 0) {
      s.setText(2, v.coverPath ?? "");
    }
    {
      const list = v.chapters ?? [];
      const items = s.initStructList(3, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        ChapterMarkerCodec.write(items[i], list[i], caps);
      }
    }
    if (v.pdfUrl !== void 0) {
      s.setText(4, v.pdfUrl ?? "");
    }
  },
  read(s, caps) {
    const out = {
      parts: s.getStructList(0, 1, 2).map((item) => PlainPartCodec.read(item, caps)),
      chapters: s.getStructList(3, 1, 1).map((item) => ChapterMarkerCodec.read(item, caps))
    };
    const m4bPathValue = s.getText(1);
    if (!(m4bPathValue === "")) {
      out.m4bPath = m4bPathValue;
    }
    const coverPathValue = s.getText(2);
    if (!(coverPathValue === "")) {
      out.coverPath = coverPathValue;
    }
    const pdfUrlValue = s.getText(4);
    if (!(pdfUrlValue === "")) {
      out.pdfUrl = pdfUrlValue;
    }
    return out;
  }
};
var FetchTitleReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PlainFetchCodec.write(s.initStruct(0, 0, 5), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("FetchTitleReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PlainFetchCodec.read(s.getStruct(0, 0, 5), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("FetchTitleReply", disc);
    }
  }
};
var SourceAccountsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.accounts ?? [];
      const items = s.initStructList(0, list.length, 1, 4);
      for (let i = 0; i < items.length; i++) {
        SourceAccountCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      accounts: s.getStructList(0, 1, 4).map((item) => SourceAccountCodec.read(item, caps))
    };
  }
};
var SourceAccountsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          SourceAccountsCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("SourceAccountsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: SourceAccountsCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("SourceAccountsReply", disc);
    }
  }
};
var SearchCatalogParamsCodec = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.query ?? "");
    s.setText(1, v.region ?? "");
    s.setUint32(0, v.limit ?? 0);
    s.setUint32(1, v.page ?? 0);
    s.setUint16(4, ord(CATALOG_SORTS, v.sort ?? CATALOG_SORTS[0], "CatalogSort"));
    s.setUint16(5, ord(CATALOG_FIELDS, v.field ?? CATALOG_FIELDS[0], "CatalogField"));
    if (v.language !== void 0) {
      s.setText(2, v.language ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      query: s.getText(0),
      region: s.getText(1),
      limit: s.getUint32(0),
      page: s.getUint32(1),
      sort: fromOrd(CATALOG_SORTS, s.getUint16(4), "CatalogSort"),
      field: fromOrd(CATALOG_FIELDS, s.getUint16(5), "CatalogField")
    };
    const languageValue = s.getText(2);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    return out;
  }
};
var ExpandCandidatesParamsCodec = {
  dataWords: 1,
  pointerCount: 10,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.source ?? "");
    s.setText(1, v.productId ?? "");
    s.setText(2, v.title ?? "");
    if (v.authors !== void 0) {
      s.setText(3, v.authors ?? "");
    }
    if (v.narrators !== void 0) {
      s.setText(4, v.narrators ?? "");
    }
    if (v.series !== void 0) {
      s.setText(5, v.series ?? "");
    }
    if (v.seriesAsin !== void 0) {
      s.setText(6, v.seriesAsin ?? "");
    }
    if (v.asin !== void 0) {
      s.setText(7, v.asin ?? "");
    }
    if (v.isbn !== void 0) {
      s.setText(8, v.isbn ?? "");
    }
    s.setText(9, v.region ?? "");
    s.setUint32(0, v.limit ?? 0);
  },
  read(s, caps) {
    void caps;
    const out = {
      source: s.getText(0),
      productId: s.getText(1),
      title: s.getText(2),
      region: s.getText(9),
      limit: s.getUint32(0)
    };
    const authorsValue = s.getText(3);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(4);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(5);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesAsinValue = s.getText(6);
    if (!(seriesAsinValue === "")) {
      out.seriesAsin = seriesAsinValue;
    }
    const asinValue = s.getText(7);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(8);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  }
};
var PurchaseHintParamsCodec = {
  dataWords: 1,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    if (v.productId !== void 0) {
      s.setText(0, v.productId ?? "");
    }
    if (v.title !== void 0) {
      s.setText(1, v.title ?? "");
    }
    if (v.authors !== void 0) {
      s.setText(2, v.authors ?? "");
    }
    if (v.asin !== void 0) {
      s.setText(3, v.asin ?? "");
    }
    if (v.isbn !== void 0) {
      s.setText(4, v.isbn ?? "");
    }
    s.setText(5, v.region ?? "");
    s.setBool(0, v.withPrice ?? false);
  },
  read(s, caps) {
    void caps;
    const out = {
      region: s.getText(5),
      withPrice: s.getBool(0)
    };
    const productIdValue = s.getText(0);
    if (!(productIdValue === "")) {
      out.productId = productIdValue;
    }
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const authorsValue = s.getText(2);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const asinValue = s.getText(3);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(4);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  }
};
var ListDealsParamsCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    if (v.limit !== void 0) {
      s.setUint32(0, v.limit ?? 0);
    }
  },
  read(s, caps) {
    void caps;
    const out = {};
    const limitValue = s.getUint32(0);
    if (!(limitValue === 0)) {
      out.limit = limitValue;
    }
    return out;
  }
};
var CatalogDetailParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    if (v.isbn !== void 0) {
      s.setText(1, v.isbn ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      productId: s.getText(0)
    };
    const isbnValue = s.getText(1);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    return out;
  }
};
var CatalogHitCodec = {
  dataWords: 5,
  pointerCount: 19,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    s.setText(1, v.title ?? "");
    if (v.authors !== void 0) {
      s.setText(2, v.authors ?? "");
    }
    if (v.narrators !== void 0) {
      s.setText(3, v.narrators ?? "");
    }
    if (v.series !== void 0) {
      s.setText(4, v.series ?? "");
    }
    if (v.seriesIndex !== void 0) {
      s.setText(5, v.seriesIndex ?? "");
    }
    if (v.asin !== void 0) {
      s.setText(6, v.asin ?? "");
    }
    if (v.isbn !== void 0) {
      s.setText(7, v.isbn ?? "");
    }
    if (v.url !== void 0) {
      s.setText(8, v.url ?? "");
    }
    if (v.coverUrl !== void 0) {
      s.setText(9, v.coverUrl ?? "");
    }
    s.setText(10, v.origin ?? "");
    if (v.subtitle !== void 0) {
      s.setText(11, v.subtitle ?? "");
    }
    if (v.description !== void 0) {
      s.setText(12, v.description ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(13, v.publisher ?? "");
    }
    if (v.lengthMinutes !== void 0) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.publishedAt !== void 0) {
      s.setText(14, v.publishedAt ?? "");
    }
    if (v.categories !== void 0) {
      s.setText(15, v.categories ?? "");
    }
    if (v.language !== void 0) {
      s.setText(16, v.language ?? "");
    }
    if (v.priceCents !== void 0) {
      s.setInt64(1, v.priceCents ?? 0);
    }
    if (v.currency !== void 0) {
      s.setText(17, v.currency ?? "");
    }
    if (v.priceLabel !== void 0) {
      s.setText(18, v.priceLabel ?? "");
    }
    if (v.ratingOverall !== void 0) {
      s.setFloat64(2, v.ratingOverall ?? 0);
    }
    if (v.ratingCount !== void 0) {
      s.setInt64(3, v.ratingCount ?? 0);
    }
    s.setUint16(16, ord(ABRIDGEMENTS, v.abridgement ?? ABRIDGEMENTS[0], "Abridgement"));
  },
  read(s, caps) {
    void caps;
    const out = {
      productId: s.getText(0),
      title: s.getText(1),
      origin: s.getText(10),
      abridgement: fromOrd(ABRIDGEMENTS, s.getUint16(16), "Abridgement")
    };
    const authorsValue = s.getText(2);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const narratorsValue = s.getText(3);
    if (!(narratorsValue === "")) {
      out.narrators = narratorsValue;
    }
    const seriesValue = s.getText(4);
    if (!(seriesValue === "")) {
      out.series = seriesValue;
    }
    const seriesIndexValue = s.getText(5);
    if (!(seriesIndexValue === "")) {
      out.seriesIndex = seriesIndexValue;
    }
    const asinValue = s.getText(6);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(7);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const urlValue = s.getText(8);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const coverUrlValue = s.getText(9);
    if (!(coverUrlValue === "")) {
      out.coverUrl = coverUrlValue;
    }
    const subtitleValue = s.getText(11);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const descriptionValue = s.getText(12);
    if (!(descriptionValue === "")) {
      out.description = descriptionValue;
    }
    const publisherValue = s.getText(13);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const publishedAtValue = s.getText(14);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const categoriesValue = s.getText(15);
    if (!(categoriesValue === "")) {
      out.categories = categoriesValue;
    }
    const languageValue = s.getText(16);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const priceCentsValue = s.getInt64(1);
    if (!(priceCentsValue === 0n)) {
      out.priceCents = priceCentsValue;
    }
    const currencyValue = s.getText(17);
    if (!(currencyValue === "")) {
      out.currency = currencyValue;
    }
    const priceLabelValue = s.getText(18);
    if (!(priceLabelValue === "")) {
      out.priceLabel = priceLabelValue;
    }
    const ratingOverallValue = s.getFloat64(2);
    if (!(ratingOverallValue === 0)) {
      out.ratingOverall = ratingOverallValue;
    }
    const ratingCountValue = s.getInt64(3);
    if (!(ratingCountValue === 0n)) {
      out.ratingCount = ratingCountValue;
    }
    return out;
  }
};
var CatalogHitsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.hits ?? [];
      const items = s.initStructList(0, list.length, 5, 19);
      for (let i = 0; i < items.length; i++) {
        CatalogHitCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      hits: s.getStructList(0, 5, 19).map((item) => CatalogHitCodec.read(item, caps))
    };
  }
};
var CatalogHitsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CatalogHitsCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CatalogHitsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CatalogHitsCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CatalogHitsReply", disc);
    }
  }
};
var CatalogDetailCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.hit != null) {
      CatalogHitCodec.write(s.initStruct(0, 5, 19), v.hit, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hit: CatalogHitCodec.read(s.getStruct(0, 5, 19), caps)
    };
  }
};
var CatalogDetailReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CatalogDetailCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CatalogDetailReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CatalogDetailCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("CatalogDetailReply", disc);
    }
  }
};
var PurchaseHintCodec = {
  dataWords: 3,
  pointerCount: 7,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.productId ?? "");
    if (v.title !== void 0) {
      s.setText(1, v.title ?? "");
    }
    if (v.url !== void 0) {
      s.setText(2, v.url ?? "");
    }
    if (v.priceCents !== void 0) {
      s.setInt64(0, v.priceCents ?? 0);
    }
    if (v.currency !== void 0) {
      s.setText(3, v.currency ?? "");
    }
    if (v.priceLabel !== void 0) {
      s.setText(4, v.priceLabel ?? "");
    }
    if (v.listPriceCents !== void 0) {
      s.setInt64(1, v.listPriceCents ?? 0);
    }
    if (v.listPriceLabel !== void 0) {
      s.setText(5, v.listPriceLabel ?? "");
    }
    if (v.memberPriceCents !== void 0) {
      s.setInt64(2, v.memberPriceCents ?? 0);
    }
    if (v.memberPriceLabel !== void 0) {
      s.setText(6, v.memberPriceLabel ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      productId: s.getText(0)
    };
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const urlValue = s.getText(2);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const priceCentsValue = s.getInt64(0);
    if (!(priceCentsValue === 0n)) {
      out.priceCents = priceCentsValue;
    }
    const currencyValue = s.getText(3);
    if (!(currencyValue === "")) {
      out.currency = currencyValue;
    }
    const priceLabelValue = s.getText(4);
    if (!(priceLabelValue === "")) {
      out.priceLabel = priceLabelValue;
    }
    const listPriceCentsValue = s.getInt64(1);
    if (!(listPriceCentsValue === 0n)) {
      out.listPriceCents = listPriceCentsValue;
    }
    const listPriceLabelValue = s.getText(5);
    if (!(listPriceLabelValue === "")) {
      out.listPriceLabel = listPriceLabelValue;
    }
    const memberPriceCentsValue = s.getInt64(2);
    if (!(memberPriceCentsValue === 0n)) {
      out.memberPriceCents = memberPriceCentsValue;
    }
    const memberPriceLabelValue = s.getText(6);
    if (!(memberPriceLabelValue === "")) {
      out.memberPriceLabel = memberPriceLabelValue;
    }
    return out;
  }
};
var PurchaseHintResultCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.hint != null) {
      PurchaseHintCodec.write(s.initStruct(0, 3, 7), v.hint, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hint: PurchaseHintCodec.read(s.getStruct(0, 3, 7), caps)
    };
  }
};
var PurchaseHintReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PurchaseHintResultCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PurchaseHintReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PurchaseHintResultCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PurchaseHintReply", disc);
    }
  }
};
var ScanLibraryParamsCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.force ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      force: s.getBool(0)
    };
  }
};
var AuthenticateUserParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.username ?? "");
    s.setText(1, v.password ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      username: s.getText(0),
      password: s.getText(1)
    };
  }
};
var ExternalUserCodec = {
  dataWords: 0,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.provider ?? "");
    s.setText(1, v.externalUserId ?? "");
    if (v.displayName !== void 0) {
      s.setText(2, v.displayName ?? "");
    }
    if (v.accessToken !== void 0) {
      s.setText(3, v.accessToken ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      provider: s.getText(0),
      externalUserId: s.getText(1)
    };
    const displayNameValue = s.getText(2);
    if (!(displayNameValue === "")) {
      out.displayName = displayNameValue;
    }
    const accessTokenValue = s.getText(3);
    if (!(accessTokenValue === "")) {
      out.accessToken = accessTokenValue;
    }
    return out;
  }
};
var ExternalUserReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ExternalUserCodec.write(s.initStruct(0, 0, 4), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExternalUserReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ExternalUserCodec.read(s.getStruct(0, 0, 4), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ExternalUserReply", disc);
    }
  }
};
var EventPollResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.users ?? [];
      const items = s.initStructList(0, list.length, 0, 4);
      for (let i = 0; i < items.length; i++) {
        ExternalUserCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      users: s.getStructList(0, 0, 4).map((item) => ExternalUserCodec.read(item, caps))
    };
  }
};
var EventPollReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          EventPollResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("EventPollReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: EventPollResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("EventPollReply", disc);
    }
  }
};
var ListeningProgressCodec = {
  dataWords: 6,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.externalUserId ?? "");
    s.setText(1, v.externalItemId ?? "");
    if (v.identityId !== void 0) {
      s.setInt64(0, v.identityId ?? 0);
    }
    if (v.title !== void 0) {
      s.setText(2, v.title ?? "");
    }
    if (v.authors !== void 0) {
      s.setText(3, v.authors ?? "");
    }
    if (v.asin !== void 0) {
      s.setText(4, v.asin ?? "");
    }
    if (v.isbn !== void 0) {
      s.setText(5, v.isbn ?? "");
    }
    if (v.progress !== void 0) {
      s.setFloat64(1, v.progress ?? 0);
    }
    if (v.currentTimeSeconds !== void 0) {
      s.setFloat64(2, v.currentTimeSeconds ?? 0);
    }
    if (v.durationSeconds !== void 0) {
      s.setFloat64(3, v.durationSeconds ?? 0);
    }
    s.setBool(256, v.isFinished ?? false);
    if (v.lastListenedAtUnixMs !== void 0) {
      s.setUint64(5, BigInt(v.lastListenedAtUnixMs ?? 0));
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      externalUserId: s.getText(0),
      externalItemId: s.getText(1),
      isFinished: s.getBool(256)
    };
    const identityIdValue = s.getInt64(0);
    if (!(identityIdValue === 0n)) {
      out.identityId = identityIdValue;
    }
    const titleValue = s.getText(2);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const authorsValue = s.getText(3);
    if (!(authorsValue === "")) {
      out.authors = authorsValue;
    }
    const asinValue = s.getText(4);
    if (!(asinValue === "")) {
      out.asin = asinValue;
    }
    const isbnValue = s.getText(5);
    if (!(isbnValue === "")) {
      out.isbn = isbnValue;
    }
    const progressValue = s.getFloat64(1);
    if (!(progressValue === 0)) {
      out.progress = progressValue;
    }
    const currentTimeSecondsValue = s.getFloat64(2);
    if (!(currentTimeSecondsValue === 0)) {
      out.currentTimeSeconds = currentTimeSecondsValue;
    }
    const durationSecondsValue = s.getFloat64(3);
    if (!(durationSecondsValue === 0)) {
      out.durationSeconds = durationSecondsValue;
    }
    const lastListenedAtUnixMsValue = Number(s.getUint64(5));
    if (!(lastListenedAtUnixMsValue === 0)) {
      out.lastListenedAtUnixMs = lastListenedAtUnixMsValue;
    }
    return out;
  }
};
var SyncListeningResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.items ?? [];
      const items = s.initStructList(0, list.length, 6, 6);
      for (let i = 0; i < items.length; i++) {
        ListeningProgressCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      items: s.getStructList(0, 6, 6).map((item) => ListeningProgressCodec.read(item, caps))
    };
  }
};
var SyncListeningReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          SyncListeningResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("SyncListeningReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: SyncListeningResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("SyncListeningReply", disc);
    }
  }
};
var DbValueCodec = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    switch (v.kind) {
      case "null":
        s.setUint16(1, 0);
        s.setUint16(0, ord(DB_TYPES, v.value ?? DB_TYPES[0], "DbType"));
        break;
      case "boolean":
        s.setUint16(1, 1);
        s.setBool(0, v.value ?? false);
        break;
      case "int64":
        s.setUint16(1, 2);
        s.setInt64(1, v.value ?? 0);
        break;
      case "float64":
        s.setUint16(1, 3);
        s.setFloat64(1, v.value ?? 0);
        break;
      case "text":
        s.setUint16(1, 4);
        s.setText(0, v.value ?? "");
        break;
      case "bytes":
        s.setUint16(1, 5);
        s.setData(0, v.value ?? EMPTY_BYTES);
        break;
      default:
        throw unknownUnion("DbValue", v.kind);
    }
  },
  read(s, caps) {
    void caps;
    const disc = s.getUint16(1);
    switch (disc) {
      case 0:
        return { kind: "null", value: fromOrd(DB_TYPES, s.getUint16(0), "DbType") };
      case 1:
        return { kind: "boolean", value: s.getBool(0) };
      case 2:
        return { kind: "int64", value: s.getInt64(1) };
      case 3:
        return { kind: "float64", value: s.getFloat64(1) };
      case 4:
        return { kind: "text", value: s.getText(0) };
      case 5:
        return { kind: "bytes", value: s.getData(0) };
      default:
        throw unknownUnion("DbValue", disc);
    }
  }
};
var DbColumnCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint16(0, ord(DB_TYPES, v.dbType ?? DB_TYPES[0], "DbType"));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      dbType: fromOrd(DB_TYPES, s.getUint16(0), "DbType")
    };
  }
};
var DbRowCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.values ?? [];
      const items = s.initStructList(0, list.length, 2, 1);
      for (let i = 0; i < items.length; i++) {
        DbValueCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      values: s.getStructList(0, 2, 1).map((item) => DbValueCodec.read(item, caps))
    };
  }
};
var SqlSpanCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.start ?? 0);
    s.setUint32(1, v.end ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      start: s.getUint32(0),
      end: s.getUint32(1)
    };
  }
};
var TextCollateSiteCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.span != null) {
      SqlSpanCodec.write(s.initStruct(0, 1, 0), v.span, caps);
    }
  },
  read(s, caps) {
    return {
      span: SqlSpanCodec.read(s.getStruct(0, 1, 0), caps)
    };
  }
};
var IntegerArithSiteCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    if (v.full != null) {
      SqlSpanCodec.write(s.initStruct(0, 1, 0), v.full, caps);
    }
    if (v.lhs != null) {
      SqlSpanCodec.write(s.initStruct(1, 1, 0), v.lhs, caps);
    }
    if (v.rhs != null) {
      SqlSpanCodec.write(s.initStruct(2, 1, 0), v.rhs, caps);
    }
    s.setUint16(0, ord(INTEGER_ARITH_KINDS, v.kind ?? INTEGER_ARITH_KINDS[0], "IntegerArithKind"));
  },
  read(s, caps) {
    return {
      full: SqlSpanCodec.read(s.getStruct(0, 1, 0), caps),
      lhs: SqlSpanCodec.read(s.getStruct(1, 1, 0), caps),
      rhs: SqlSpanCodec.read(s.getStruct(2, 1, 0), caps),
      kind: fromOrd(INTEGER_ARITH_KINDS, s.getUint16(0), "IntegerArithKind")
    };
  }
};
var PhysicalAccessCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setText(1, v.column ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      column: s.getText(1)
    };
  }
};
var ResolvedAssignmentCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setText(1, v.column ?? "");
    s.setUint16(0, ord(RESOLVED_SQL_TYPES, v.dest ?? RESOLVED_SQL_TYPES[0], "ResolvedSqlType"));
    s.setUint16(1, ord(RESOLVED_SQL_TYPES, v.source ?? RESOLVED_SQL_TYPES[0], "ResolvedSqlType"));
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      column: s.getText(1),
      dest: fromOrd(RESOLVED_SQL_TYPES, s.getUint16(0), "ResolvedSqlType"),
      source: fromOrd(RESOLVED_SQL_TYPES, s.getUint16(1), "ResolvedSqlType")
    };
  }
};
var NamedSqlTypeCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint16(0, ord(RESOLVED_SQL_TYPES, v.sqlType ?? RESOLVED_SQL_TYPES[0], "ResolvedSqlType"));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      sqlType: fromOrd(RESOLVED_SQL_TYPES, s.getUint16(0), "ResolvedSqlType")
    };
  }
};
var ColumnReferenceCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.refTable ?? "");
    s.setTextList(1, v.refColumns ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      refTable: s.getText(0),
      refColumns: s.getTextList(1)
    };
  }
};
var OptionalColumnReferenceCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "none":
        s.setUint16(0, 0);
        break;
      case "some":
        s.setUint16(0, 1);
        if (v.value != null) {
          ColumnReferenceCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("OptionalColumnReference", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "none" };
      case 1:
        return { kind: "some", value: ColumnReferenceCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("OptionalColumnReference", disc);
    }
  }
};
var ForeignKeyConstraintCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.columns ?? []);
    s.setText(1, v.refTable ?? "");
    s.setTextList(2, v.refColumns ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      columns: s.getTextList(0),
      refTable: s.getText(1),
      refColumns: s.getTextList(2)
    };
  }
};
var TableConstraintCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "primaryKey":
        s.setUint16(0, 0);
        s.setTextList(0, v.value ?? []);
        break;
      case "unique":
        s.setUint16(0, 1);
        s.setTextList(0, v.value ?? []);
        break;
      case "check":
        s.setUint16(0, 2);
        s.setText(0, v.value ?? "");
        break;
      case "foreignKey":
        s.setUint16(0, 3);
        if (v.value != null) {
          ForeignKeyConstraintCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("TableConstraint", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "primaryKey", value: s.getTextList(0) };
      case 1:
        return { kind: "unique", value: s.getTextList(0) };
      case 2:
        return { kind: "check", value: s.getText(0) };
      case 3:
        return { kind: "foreignKey", value: ForeignKeyConstraintCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("TableConstraint", disc);
    }
  }
};
var CreateTableSchemaCodec = {
  dataWords: 0,
  pointerCount: 10,
  write(s, v, caps) {
    s.setText(0, v.table ?? "");
    {
      const list = v.columns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        NamedSqlTypeCodec.write(items[i], list[i], caps);
      }
    }
    s.setText(2, v.identityColumn ?? "");
    s.setBoolList(3, v.columnNotNull ?? []);
    s.setBoolList(4, v.columnUnique ?? []);
    s.setBoolList(5, v.columnPrimaryKey ?? []);
    s.setTextList(6, v.columnDefaults ?? []);
    s.setTextList(7, v.columnChecks ?? []);
    {
      const list = v.columnReferences ?? [];
      const items = s.initStructList(8, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        OptionalColumnReferenceCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.tableConstraints ?? [];
      const items = s.initStructList(9, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        TableConstraintCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      table: s.getText(0),
      columns: s.getStructList(1, 1, 1).map((item) => NamedSqlTypeCodec.read(item, caps)),
      identityColumn: s.getText(2),
      columnNotNull: s.getBoolList(3),
      columnUnique: s.getBoolList(4),
      columnPrimaryKey: s.getBoolList(5),
      columnDefaults: s.getTextList(6),
      columnChecks: s.getTextList(7),
      columnReferences: s.getStructList(8, 1, 1).map((item) => OptionalColumnReferenceCodec.read(item, caps)),
      tableConstraints: s.getStructList(9, 1, 1).map((item) => TableConstraintCodec.read(item, caps))
    };
  }
};
var SchemaCreateCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.schema != null) {
      CreateTableSchemaCodec.write(s.initStruct(0, 0, 10), v.schema, caps);
    }
    s.setText(1, v.fingerprint ?? "");
    s.setBool(0, v.noop ?? false);
  },
  read(s, caps) {
    return {
      schema: CreateTableSchemaCodec.read(s.getStruct(0, 0, 10), caps),
      fingerprint: s.getText(1),
      noop: s.getBool(0)
    };
  }
};
var SchemaActionCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "none":
        s.setUint16(0, 0);
        break;
      case "create":
        s.setUint16(0, 1);
        if (v.value != null) {
          SchemaCreateCodec.write(s.initStruct(0, 1, 2), v.value, caps);
        }
        break;
      case "drop":
        s.setUint16(0, 2);
        s.setText(0, v.value ?? "");
        break;
      default:
        throw unknownUnion("SchemaAction", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "none" };
      case 1:
        return { kind: "create", value: SchemaCreateCodec.read(s.getStruct(0, 1, 2), caps) };
      case 2:
        return { kind: "drop", value: s.getText(0) };
      default:
        throw unknownUnion("SchemaAction", disc);
    }
  }
};
var ResolvedStatementCodec = {
  dataWords: 0,
  pointerCount: 8,
  write(s, v, caps) {
    s.setText(0, v.statementHash ?? "");
    {
      const list = v.outputColumns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        NamedSqlTypeCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.physicalAccesses ?? [];
      const items = s.initStructList(2, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        PhysicalAccessCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.assignments ?? [];
      const items = s.initStructList(3, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        ResolvedAssignmentCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.textCollateSites ?? [];
      const items = s.initStructList(4, list.length, 0, 1);
      for (let i = 0; i < items.length; i++) {
        TextCollateSiteCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.integerArithSites ?? [];
      const items = s.initStructList(5, list.length, 1, 3);
      for (let i = 0; i < items.length; i++) {
        IntegerArithSiteCodec.write(items[i], list[i], caps);
      }
    }
    s.setTextList(6, v.functions ?? []);
    if (v.schemaAction != null) {
      SchemaActionCodec.write(s.initStruct(7, 1, 1), v.schemaAction, caps);
    }
  },
  read(s, caps) {
    return {
      statementHash: s.getText(0),
      outputColumns: s.getStructList(1, 1, 1).map((item) => NamedSqlTypeCodec.read(item, caps)),
      physicalAccesses: s.getStructList(2, 0, 2).map((item) => PhysicalAccessCodec.read(item, caps)),
      assignments: s.getStructList(3, 1, 2).map((item) => ResolvedAssignmentCodec.read(item, caps)),
      textCollateSites: s.getStructList(4, 0, 1).map((item) => TextCollateSiteCodec.read(item, caps)),
      integerArithSites: s.getStructList(5, 1, 3).map((item) => IntegerArithSiteCodec.read(item, caps)),
      functions: s.getTextList(6),
      schemaAction: SchemaActionCodec.read(s.getStruct(7, 1, 1), caps)
    };
  }
};
var AdapterReceiptCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.guestLen ?? 0);
    s.setText(0, v.guestHash ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      guestLen: s.getUint32(0),
      guestHash: s.getText(0)
    };
  }
};
var AdapterStatementCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.sql ?? "");
    {
      const list = v.parameters ?? [];
      const items = s.initStructList(1, list.length, 2, 1);
      for (let i = 0; i < items.length; i++) {
        DbValueCodec.write(items[i], list[i], caps);
      }
    }
    s.setUint16(0, ord(DB_STATEMENT_KINDS, v.kind ?? DB_STATEMENT_KINDS[0], "DbStatementKind"));
    s.setUint32(1, v.maxRows ?? 0);
    s.setUint16(1, ord(DB_RESULT_SELECTIONS, v.resultSelection ?? DB_RESULT_SELECTIONS[0], "DbResultSelection"));
    if (v.proof != null) {
      ResolvedStatementCodec.write(s.initStruct(2, 0, 8), v.proof, caps);
    }
  },
  read(s, caps) {
    return {
      sql: s.getText(0),
      parameters: s.getStructList(1, 2, 1).map((item) => DbValueCodec.read(item, caps)),
      kind: fromOrd(DB_STATEMENT_KINDS, s.getUint16(0), "DbStatementKind"),
      maxRows: s.getUint32(1),
      resultSelection: fromOrd(DB_RESULT_SELECTIONS, s.getUint16(1), "DbResultSelection"),
      proof: ResolvedStatementCodec.read(s.getStruct(2, 0, 8), caps)
    };
  }
};
var AdapterExecuteRequestCodec = {
  dataWords: 2,
  pointerCount: 4,
  write(s, v, caps) {
    s.setText(0, v.operationId ?? "");
    s.setText(1, v.requestHash ?? "");
    {
      const list = v.statements ?? [];
      const items = s.initStructList(2, list.length, 1, 3);
      for (let i = 0; i < items.length; i++) {
        AdapterStatementCodec.write(items[i], list[i], caps);
      }
    }
    s.setUint64(0, BigInt(v.deadlineUnixMs ?? 0));
    s.setUint16(4, ord(ISOLATION_REQS, v.isolation ?? ISOLATION_REQS[0], "IsolationReq"));
    if (v.receipt != null) {
      AdapterReceiptCodec.write(s.initStruct(3, 1, 1), v.receipt, caps);
    }
  },
  read(s, caps) {
    return {
      operationId: s.getText(0),
      requestHash: s.getText(1),
      statements: s.getStructList(2, 1, 3).map((item) => AdapterStatementCodec.read(item, caps)),
      deadlineUnixMs: Number(s.getUint64(0)),
      isolation: fromOrd(ISOLATION_REQS, s.getUint16(4), "IsolationReq"),
      receipt: AdapterReceiptCodec.read(s.getStruct(3, 1, 1), caps)
    };
  }
};
var StatementResultCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.rows ?? [];
      const items = s.initStructList(0, list.length, 0, 1);
      for (let i = 0; i < items.length; i++) {
        DbRowCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.columns ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        DbColumnCodec.write(items[i], list[i], caps);
      }
    }
    s.setUint64(0, BigInt(v.rowsAffected ?? 0));
  },
  read(s, caps) {
    return {
      rows: s.getStructList(0, 0, 1).map((item) => DbRowCodec.read(item, caps)),
      columns: s.getStructList(1, 1, 1).map((item) => DbColumnCodec.read(item, caps)),
      rowsAffected: Number(s.getUint64(0))
    };
  }
};
var DbTimingCodec = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint64(0, BigInt(v.attemptElapsedUs ?? 0));
    s.setUint64(1, BigInt(v.dbExecutionUs ?? 0));
    s.setText(0, v.dbTimingSource ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      attemptElapsedUs: Number(s.getUint64(0)),
      dbExecutionUs: Number(s.getUint64(1)),
      dbTimingSource: s.getText(0)
    };
  }
};
var ExecuteReplyCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.operationId ?? "");
    {
      const list = v.statements ?? [];
      const items = s.initStructList(1, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        StatementResultCodec.write(items[i], list[i], caps);
      }
    }
    if (v.timing != null) {
      DbTimingCodec.write(s.initStruct(2, 2, 1), v.timing, caps);
    }
  },
  read(s, caps) {
    return {
      operationId: s.getText(0),
      statements: s.getStructList(1, 1, 2).map((item) => StatementResultCodec.read(item, caps)),
      timing: DbTimingCodec.read(s.getStruct(2, 2, 1), caps)
    };
  }
};
var ExecuteResultReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ExecuteReplyCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExecuteResultReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ExecuteReplyCodec.read(s.getStruct(0, 0, 3), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("ExecuteResultReply", disc);
    }
  }
};
var DbCapabilitiesCodec = {
  dataWords: 7,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.sqlContractVersion ?? 0);
    s.setBool(32, v.atomicBatch ?? false);
    s.setBool(33, v.returning ?? false);
    s.setBool(34, v.affectedRows ?? false);
    s.setBool(35, v.schemaMigrations ?? false);
    s.setBool(36, v.cancellation ?? false);
    s.setBool(37, v.timing ?? false);
    s.setUint32(2, v.maxBinds ?? 0);
    s.setUint32(3, v.maxStatements ?? 0);
    s.setUint32(4, v.maxResultRows ?? 0);
    s.setUint32(5, v.maxPayloadBytes ?? 0);
    s.setUint32(6, v.maxResultBytes ?? 0);
    s.setUint32(7, v.maxCellBytes ?? 0);
    s.setUint32(8, v.maxRequestBytes ?? 0);
    s.setUint32(9, v.maxAtomicResultBytes ?? 0);
    s.setBool(38, v.pluginDatabases ?? false);
    s.setUint32(10, v.maxFunctionArgs ?? 0);
    s.setUint32(11, v.maxSchemaColumns ?? 0);
    s.setUint32(12, v.maxPatternBytes ?? 0);
    s.setUint32(13, v.maxLoweredStatementBytes ?? 0);
    s.setBool(39, v.consistentBackupRead ?? false);
    s.setBool(40, v.atomicUnitRestore ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      sqlContractVersion: s.getUint32(0),
      atomicBatch: s.getBool(32),
      returning: s.getBool(33),
      affectedRows: s.getBool(34),
      schemaMigrations: s.getBool(35),
      cancellation: s.getBool(36),
      timing: s.getBool(37),
      maxBinds: s.getUint32(2),
      maxStatements: s.getUint32(3),
      maxResultRows: s.getUint32(4),
      maxPayloadBytes: s.getUint32(5),
      maxResultBytes: s.getUint32(6),
      maxCellBytes: s.getUint32(7),
      maxRequestBytes: s.getUint32(8),
      maxAtomicResultBytes: s.getUint32(9),
      pluginDatabases: s.getBool(38),
      maxFunctionArgs: s.getUint32(10),
      maxSchemaColumns: s.getUint32(11),
      maxPatternBytes: s.getUint32(12),
      maxLoweredStatementBytes: s.getUint32(13),
      consistentBackupRead: s.getBool(39),
      atomicUnitRestore: s.getBool(40)
    };
  }
};
var DbBootstrapReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DbBootstrapCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DbBootstrapReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DbBootstrapCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DbBootstrapReply", disc);
    }
  }
};
var DbBootstrapCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.engine ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      engine: s.getText(0)
    };
  }
};
var DbCapabilitiesReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DbCapabilitiesCodec.write(s.initStruct(0, 7, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DbCapabilitiesReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DbCapabilitiesCodec.read(s.getStruct(0, 7, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("DbCapabilitiesReply", disc);
    }
  }
};
var IdentityHighWaterCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.table ?? "");
    s.setInt64(0, v.last ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      table: s.getText(0),
      last: s.getInt64(0)
    };
  }
};
var IdentityExportReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 1, 1);
          for (let i = 0; i < items.length; i++) {
            IdentityHighWaterCodec.write(items[i], list[i], caps);
          }
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("IdentityExportReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getStructList(0, 1, 1).map((item) => IdentityHighWaterCodec.read(item, caps)) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("IdentityExportReply", disc);
    }
  }
};
var UserRelationsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        s.setTextList(0, v.value ?? []);
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("UserRelationsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: s.getTextList(0) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("UserRelationsReply", disc);
    }
  }
};
var PluginMigrationOpCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    switch (v.kind) {
      case "schema":
        s.setUint16(0, 0);
        s.setText(0, v.value ?? "");
        break;
      case "data":
        s.setUint16(0, 1);
        s.setText(0, v.value ?? "");
        break;
      default:
        throw unknownUnion("PluginMigrationOp", v.kind);
    }
  },
  read(s, caps) {
    void caps;
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "schema", value: s.getText(0) };
      case 1:
        return { kind: "data", value: s.getText(0) };
      default:
        throw unknownUnion("PluginMigrationOp", disc);
    }
  }
};
var PluginMigrationCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.id ?? "");
    {
      const list = v.operations ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        PluginMigrationOpCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      id: s.getText(0),
      operations: s.getStructList(1, 1, 1).map((item) => PluginMigrationOpCodec.read(item, caps))
    };
  }
};
var PluginMigrationsOkCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.migrations ?? [];
      const items = s.initStructList(0, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        PluginMigrationCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      migrations: s.getStructList(0, 0, 2).map((item) => PluginMigrationCodec.read(item, caps))
    };
  }
};
var PluginMigrationsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PluginMigrationsOkCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PluginMigrationsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PluginMigrationsOkCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("PluginMigrationsReply", disc);
    }
  }
};
var DestinationHeadParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0)
    };
  }
};
var DestinationHeadResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HeadReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HeadReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationListParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.options != null) {
      ListOptionsCodec.write(s.initStruct(0, 1, 2), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      options: ListOptionsCodec.read(s.getStruct(0, 1, 2), caps)
    };
  }
};
var DestinationListResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ListReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ListReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationCopyParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.from ?? "");
    s.setText(1, v.to ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      from: s.getText(0),
      to: s.getText(1)
    };
  }
};
var DestinationCopyResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CopyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CopyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationDeleteParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0)
    };
  }
};
var DestinationDeleteResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationCommitParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setText(1, v.commitToken ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      commitToken: s.getText(1)
    };
  }
};
var DestinationCommitResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PutReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PutReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationAbortStageParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setText(1, v.commitToken ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      commitToken: s.getText(1)
    };
  }
};
var DestinationAbortStageResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var JobRunnerJobParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.controller != null) {
      JobControllerCodec.write(s.initStruct(0, 0, 5), v.controller, caps);
    }
  },
  read(s, caps) {
    return {
      controller: JobControllerCodec.read(s.getStruct(0, 0, 5), caps)
    };
  }
};
var JobRunnerJobResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HandleReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HandleReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var EventConsumerEventParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.batch != null) {
      EventBatchCodec.write(s.initStruct(0, 0, 1), v.batch, caps);
    }
  },
  read(s, caps) {
    return {
      batch: EventBatchCodec.read(s.getStruct(0, 0, 1), caps)
    };
  }
};
var EventConsumerEventResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EventBatchReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EventBatchReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceLoginParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginParamsCodec.write(s.initStruct(0, 2, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginParamsCodec.read(s.getStruct(0, 2, 10), caps)
    };
  }
};
var ContentSourceLoginResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceScanParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ScanParamsCodec.write(s.initStruct(0, 1, 3), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ScanParamsCodec.read(s.getStruct(0, 1, 3), caps)
    };
  }
};
var ContentSourceScanResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ScanReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ScanReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceFetchTitleParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      FetchTitleParamsCodec.write(s.initStruct(0, 0, 7), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: FetchTitleParamsCodec.read(s.getStruct(0, 0, 7), caps)
    };
  }
};
var ContentSourceFetchTitleResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      FetchTitleReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: FetchTitleReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceListAccountsParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var ContentSourceListAccountsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      SourceAccountsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: SourceAccountsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceLoginStartParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginParamsCodec.write(s.initStruct(0, 2, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginParamsCodec.read(s.getStruct(0, 2, 10), caps)
    };
  }
};
var ContentSourceLoginStartResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginStartReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginStartReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceLoginCompleteParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      LoginCompleteParamsCodec.write(s.initStruct(0, 0, 1), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: LoginCompleteParamsCodec.read(s.getStruct(0, 0, 1), caps)
    };
  }
};
var ContentSourceLoginCompleteResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      LoginReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: LoginReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceSearchCatalogParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      SearchCatalogParamsCodec.write(s.initStruct(0, 2, 3), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: SearchCatalogParamsCodec.read(s.getStruct(0, 2, 3), caps)
    };
  }
};
var ContentSourceSearchCatalogResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceExpandCandidatesParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ExpandCandidatesParamsCodec.write(s.initStruct(0, 1, 10), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ExpandCandidatesParamsCodec.read(s.getStruct(0, 1, 10), caps)
    };
  }
};
var ContentSourceExpandCandidatesResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourcePurchaseHintParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      PurchaseHintParamsCodec.write(s.initStruct(0, 1, 6), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: PurchaseHintParamsCodec.read(s.getStruct(0, 1, 6), caps)
    };
  }
};
var ContentSourcePurchaseHintResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PurchaseHintReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PurchaseHintReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceListDealsParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ListDealsParamsCodec.write(s.initStruct(0, 1, 0), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ListDealsParamsCodec.read(s.getStruct(0, 1, 0), caps)
    };
  }
};
var ContentSourceListDealsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogHitsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogHitsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceHealthParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var ContentSourceHealthResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HealthReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HealthReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceDiagnoseParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var ContentSourceDiagnoseResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DiagnoseReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DiagnoseReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceCatalogDetailParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      CatalogDetailParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: CatalogDetailParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var ContentSourceCatalogDetailResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CatalogDetailReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CatalogDetailReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryHealthParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibraryHealthResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HealthReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HealthReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryStartParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibraryStartResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryStopParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibraryStopResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryDiagnoseParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibraryDiagnoseResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DiagnoseReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DiagnoseReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryScanLibraryParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      ScanLibraryParamsCodec.write(s.initStruct(0, 1, 0), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ScanLibraryParamsCodec.read(s.getStruct(0, 1, 0), caps)
    };
  }
};
var RemoteLibraryScanLibraryResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibrarySyncListeningParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibrarySyncListeningResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      SyncListeningReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: SyncListeningReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var RemoteLibraryPollEventsParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var RemoteLibraryPollEventsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EventPollReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EventPollReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var PluginCliDescribeParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var PluginCliDescribeResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CliSchemaReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CliSchemaReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var PluginCliInvokeParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      CliInvokeParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: CliInvokeParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var PluginCliInvokeResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CliInvokeReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CliInvokeReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var OidcClientsParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var OidcClientsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      OidcClientsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: OidcClientsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var OidcAuthenticateUserParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      AuthenticateUserParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: AuthenticateUserParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var OidcAuthenticateUserResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExternalUserReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExternalUserReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DatabaseOpenSessionParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var DatabaseOpenSessionResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      AdapterSessionReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: AdapterSessionReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionCapabilitiesParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionCapabilitiesResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DbCapabilitiesReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DbCapabilitiesReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionExecuteParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.request != null) {
      AdapterExecuteRequestCodec.write(s.initStruct(0, 2, 4), v.request, caps);
    }
  },
  read(s, caps) {
    return {
      request: AdapterExecuteRequestCodec.read(s.getStruct(0, 2, 4), caps)
    };
  }
};
var AdapterDatabaseSessionExecuteResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExecuteResultReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExecuteResultReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionCloseParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionCloseResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionBootstrapParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionBootstrapResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DbBootstrapReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DbBootstrapReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionExportIdentityParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionExportIdentityResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      IdentityExportReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: IdentityExportReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionImportIdentityParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.rows ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        IdentityHighWaterCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      rows: s.getStructList(0, 1, 1).map((item) => IdentityHighWaterCodec.read(item, caps))
    };
  }
};
var AdapterDatabaseSessionImportIdentityResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionListUserRelationsParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionListUserRelationsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      UserRelationsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: UserRelationsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionPrepareUnitRestoreParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionPrepareUnitRestoreResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionDropUserRelationsParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.names ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      names: s.getTextList(0)
    };
  }
};
var AdapterDatabaseSessionDropUserRelationsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AdapterDatabaseSessionAssertRestoreConstraintsParamsCodec = {
  dataWords: 0,
  pointerCount: 0,
  write(s, v, caps) {
    void s;
    void v;
    void caps;
  },
  read(s, caps) {
    void caps;
    void s;
    return {};
  }
};
var AdapterDatabaseSessionAssertRestoreConstraintsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      EmptyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: EmptyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var PluginWorkerDatabaseMigrationsParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.binding ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      binding: s.getText(0)
    };
  }
};
var PluginWorkerDatabaseMigrationsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PluginMigrationsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PluginMigrationsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};

// dist/invoke.js
var InvokeError = class extends PluginError {
  /** HTTP status the bridge answers with. */
  status;
  constructor(status, code, message) {
    super(code, message);
    this.name = "InvokeError";
    this.status = status;
  }
};
function okValue(value) {
  return { kind: "ok", value };
}
function okPass(value) {
  return { kind: "ok", value };
}
function okEmpty() {
  return { kind: "ok" };
}
function named(stub, ctx, method, args) {
  return stub.bookclerkInvoke(ctx, method, ...args);
}
function spec(entry) {
  return entry;
}
function withParams(stub, method, params, results, ok) {
  return spec({
    params,
    results,
    stub,
    call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), method, [p.params]),
    ok
  });
}
function withoutParams(stub, method, params, results, ok) {
  return spec({
    params,
    results,
    stub,
    call: (s, _p, cx) => named(s, cx.host.bindContext(cx.ctx), method, []),
    ok
  });
}
var asList = (value) => Array.isArray(value) ? value : [];
var okHits = (value) => okValue({ hits: asList(value) });
var okLines = (value) => okValue({ lines: asList(value).map((l) => String(l)) });
var okHealth = (value) => {
  const v = value ?? {};
  return okValue({ ok: Boolean(v.ok), detail: String(v.detail ?? "") });
};
function eventResult(outcome) {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack", value: { dummy: void 0 } };
    case "retry":
      return {
        kind: "retry",
        value: { retryAtUnixMs: Number(outcome.retryAtUnixMs) || 0, reason: String(outcome.reason ?? "") }
      };
    case "reject":
      return { kind: "reject", value: { reason: String(outcome.reason ?? "") } };
    case "deadLetter":
      return { kind: "deadLetter", value: { reason: String(outcome.reason ?? "") } };
    case "suspended":
      return {
        kind: "suspended",
        value: {
          checkpointJson: String(outcome.checkpointJson ?? ""),
          checkpointSchemaVersion: Number(outcome.checkpointSchemaVersion) || 0,
          wakeAtUnixMs: Number(outcome.wakeAtUnixMs) || 0,
          wakeOnEventType: String(outcome.wakeOnEventType ?? ""),
          wakeOnFilterJson: String(outcome.wakeOnFilterJson ?? "")
        }
      };
    default:
      throw PluginError.fromWire("internal", `unknown event outcome ${String(outcome.kind)}`);
  }
}
function jobOutcome(record) {
  switch (record.kind) {
    case "completed":
      return {
        kind: "completed",
        value: { message: String(record.message ?? ""), bytesCopied: Number(record.bytesCopied) || 0 }
      };
    case "retryable":
      return {
        kind: "retryable",
        value: { message: String(record.message ?? ""), retryAfterUnixMs: Number(record.retryAfterUnixMs) || 0 }
      };
    case "rejected":
      return { kind: "rejected", value: { message: String(record.message ?? "") } };
    case "cancelled":
      return { kind: "cancelled", value: { message: String(record.message ?? "") } };
    case "suspended":
      return {
        kind: "suspended",
        value: {
          checkpointJson: String(record.checkpoint?.json ?? ""),
          checkpointSchemaVersion: Number(record.checkpoint?.schemaVersion) || 0,
          wakeAtUnixMs: Number(record.wakeAtUnixMs) || 0
        }
      };
    default:
      throw PluginError.fromWire("internal", `unknown job outcome ${String(record.kind)}`);
  }
}
function wireMigration(migration) {
  const ops = Array.isArray(migration?.operations) ? migration.operations : [];
  return {
    id: String(migration?.id ?? ""),
    operations: ops.map((op) => {
      const o = op ?? {};
      if (typeof o.schema === "string")
        return { kind: "schema", value: o.schema };
      if (typeof o.data === "string")
        return { kind: "data", value: o.data };
      if (o.kind === "schema" || o.kind === "data")
        return { kind: o.kind, value: String(o.value ?? "") };
      throw PluginError.fromWire("invalid_params", `migration \`${String(migration?.id)}\` has an op without schema/data`);
    })
  };
}
function listOptions(options) {
  return {
    prefix: options?.prefix || void 0,
    cursor: options?.cursor || void 0,
    limit: options?.limit || void 0
  };
}
function sessionCall(method) {
  return (s, p, cx) => {
    if (!cx.target) {
      throw new InvokeError(400, "invalid_params", `AdapterDatabaseSession.${method} requires X-Bookclerk-Target`);
    }
    const params = p;
    const args = method === "execute" ? [params.request] : method === "importIdentity" ? [params.rows] : method === "dropUserRelations" ? [params.names] : [];
    return named(s, cx.host.bindContext(cx.ctx), "session", [cx.target, method, ...args]);
  };
}
function session(method, params, results, ok) {
  return spec({ params, results, stub: "databaseAdapter", call: sessionCall(method), ok });
}
var METHODS = {
  PluginWorker: {
    databaseMigrations: spec({
      params: PluginWorkerDatabaseMigrationsParamsCodec,
      results: PluginWorkerDatabaseMigrationsResultsCodec,
      stub: "author",
      call: (s, p) => s.bookclerkDatabaseMigrations(String(p.binding ?? "")),
      ok: (value) => okValue({ migrations: asList(value).map(wireMigration) })
    })
  },
  ContentSource: {
    login: withParams("storefront", "login", ContentSourceLoginParamsCodec, ContentSourceLoginResultsCodec, okPass),
    scan: withParams("storefront", "scan", ContentSourceScanParamsCodec, ContentSourceScanResultsCodec, okPass),
    fetchTitle: withParams("storefront", "fetchTitle", ContentSourceFetchTitleParamsCodec, ContentSourceFetchTitleResultsCodec, okPass),
    listAccounts: withoutParams("storefront", "listAccounts", ContentSourceListAccountsParamsCodec, ContentSourceListAccountsResultsCodec, (value) => okValue({ accounts: asList(value) })),
    loginStart: withParams("storefront", "loginStart", ContentSourceLoginStartParamsCodec, ContentSourceLoginStartResultsCodec, okPass),
    loginComplete: withParams("storefront", "loginComplete", ContentSourceLoginCompleteParamsCodec, ContentSourceLoginCompleteResultsCodec, okPass),
    searchCatalog: withParams("storefront", "searchCatalog", ContentSourceSearchCatalogParamsCodec, ContentSourceSearchCatalogResultsCodec, okHits),
    expandCandidates: withParams("storefront", "expandCandidates", ContentSourceExpandCandidatesParamsCodec, ContentSourceExpandCandidatesResultsCodec, okHits),
    purchaseHint: withParams("storefront", "purchaseHint", ContentSourcePurchaseHintParamsCodec, ContentSourcePurchaseHintResultsCodec, (value) => okValue({ found: value != null, hint: value ?? void 0 })),
    listDeals: withParams("storefront", "listDeals", ContentSourceListDealsParamsCodec, ContentSourceListDealsResultsCodec, okHits),
    health: withoutParams("storefront", "health", ContentSourceHealthParamsCodec, ContentSourceHealthResultsCodec, okHealth),
    diagnose: withoutParams("storefront", "diagnose", ContentSourceDiagnoseParamsCodec, ContentSourceDiagnoseResultsCodec, okLines),
    catalogDetail: withParams("storefront", "catalogDetail", ContentSourceCatalogDetailParamsCodec, ContentSourceCatalogDetailResultsCodec, (value) => okValue({ found: value != null, hit: value ?? void 0 }))
  },
  Destination: {
    head: spec({
      params: DestinationHeadParamsCodec,
      results: DestinationHeadResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "head", [String(p.key ?? "")]),
      ok: (value) => {
        const meta = value ?? null;
        return okValue({ found: meta != null, meta: meta ?? void 0 });
      }
    }),
    list: spec({
      params: DestinationListParamsCodec,
      results: DestinationListResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "list", [listOptions(p.options)]),
      ok: okPass
    }),
    copy: spec({
      params: DestinationCopyParamsCodec,
      results: DestinationCopyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "copy", [String(p.from ?? ""), String(p.to ?? "")]),
      ok: okPass
    }),
    delete: spec({
      params: DestinationDeleteParamsCodec,
      results: DestinationDeleteResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "delete", [String(p.key ?? "")]),
      ok: okEmpty
    }),
    commit: spec({
      params: DestinationCommitParamsCodec,
      results: DestinationCommitResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "commit", [String(p.key ?? ""), String(p.commitToken ?? "")]),
      ok: okPass
    }),
    abortStage: spec({
      params: DestinationAbortStageParamsCodec,
      results: DestinationAbortStageResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "abortStage", [String(p.key ?? ""), String(p.commitToken ?? "")]),
      ok: okEmpty
    })
  },
  RemoteLibrary: {
    health: withoutParams("remoteLibrary", "health", RemoteLibraryHealthParamsCodec, RemoteLibraryHealthResultsCodec, okHealth),
    start: withoutParams("remoteLibrary", "start", RemoteLibraryStartParamsCodec, RemoteLibraryStartResultsCodec, okEmpty),
    stop: withoutParams("remoteLibrary", "stop", RemoteLibraryStopParamsCodec, RemoteLibraryStopResultsCodec, okEmpty),
    diagnose: withoutParams("remoteLibrary", "diagnose", RemoteLibraryDiagnoseParamsCodec, RemoteLibraryDiagnoseResultsCodec, okLines),
    scanLibrary: withParams("remoteLibrary", "scanLibrary", RemoteLibraryScanLibraryParamsCodec, RemoteLibraryScanLibraryResultsCodec, okEmpty),
    syncListening: withoutParams("remoteLibrary", "syncListening", RemoteLibrarySyncListeningParamsCodec, RemoteLibrarySyncListeningResultsCodec, (value) => okValue({ items: asList(value) })),
    pollEvents: withoutParams("remoteLibrary", "pollEvents", RemoteLibraryPollEventsParamsCodec, RemoteLibraryPollEventsResultsCodec, (value) => okValue({ users: asList(value) }))
  },
  EventConsumer: {
    event: spec({
      params: EventConsumerEventParamsCodec,
      results: EventConsumerEventResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkEvent(cx.host.bindContext(cx.ctx), {
        events: Array.isArray(p.batch?.events) ? p.batch.events : []
      }),
      ok: (value) => okValue(asList(value).map(eventResult))
    })
  },
  JobRunner: {
    job: spec({
      params: JobRunnerJobParamsCodec,
      results: JobRunnerJobResultsCodec,
      stub: "author",
      call: (s, p, cx) => {
        const controller = p.controller ?? {};
        const granted = {
          input: controller.input ?? null,
          output: controller.output ?? null,
          progress: controller.progress ?? null,
          cancel: controller.cancel ?? null
        };
        return s.bookclerkJob(cx.host.bindContext(cx.ctx), controller.invocation ?? {}, granted);
      },
      ok: (value) => okValue(jobOutcome(value))
    })
  },
  PluginCli: {
    describe: withoutParams("cli", "describe", PluginCliDescribeParamsCodec, PluginCliDescribeResultsCodec, okPass),
    invoke: withParams("cli", "invoke", PluginCliInvokeParamsCodec, PluginCliInvokeResultsCodec, okPass)
  },
  Oidc: {
    clients: withoutParams("oidc", "clients", OidcClientsParamsCodec, OidcClientsResultsCodec, (value) => okValue({ clients: asList(value) })),
    authenticateUser: withParams("oidc", "authenticateUser", OidcAuthenticateUserParamsCodec, OidcAuthenticateUserResultsCodec, okPass)
  },
  Database: {
    openSession: spec({
      params: DatabaseOpenSessionParamsCodec,
      results: DatabaseOpenSessionResultsCodec,
      stub: "databaseAdapter",
      call: async (s, _p, cx) => {
        const id = await named(s, cx.host.bindContext(cx.ctx), "openSession", []);
        if (typeof id !== "string" || !id) {
          throw PluginError.fromWire("internal", "openSession returned no session id");
        }
        return id;
      },
      // The session id is exported through the reply CapTable as `adapterSession`.
      ok: (value) => okValue(value)
    })
  },
  AdapterDatabaseSession: {
    capabilities: session("capabilities", AdapterDatabaseSessionCapabilitiesParamsCodec, AdapterDatabaseSessionCapabilitiesResultsCodec, (value) => okValue(value)),
    execute: session("execute", AdapterDatabaseSessionExecuteParamsCodec, AdapterDatabaseSessionExecuteResultsCodec, (value) => okValue(value)),
    close: session("close", AdapterDatabaseSessionCloseParamsCodec, AdapterDatabaseSessionCloseResultsCodec, okEmpty),
    bootstrap: session("bootstrap", AdapterDatabaseSessionBootstrapParamsCodec, AdapterDatabaseSessionBootstrapResultsCodec, (value) => okValue(value)),
    exportIdentity: session("exportIdentity", AdapterDatabaseSessionExportIdentityParamsCodec, AdapterDatabaseSessionExportIdentityResultsCodec, (value) => okValue(asList(value))),
    importIdentity: session("importIdentity", AdapterDatabaseSessionImportIdentityParamsCodec, AdapterDatabaseSessionImportIdentityResultsCodec, okEmpty),
    listUserRelations: session("listUserRelations", AdapterDatabaseSessionListUserRelationsParamsCodec, AdapterDatabaseSessionListUserRelationsResultsCodec, (value) => okValue(asList(value).map((n) => String(n)))),
    prepareUnitRestore: session("prepareUnitRestore", AdapterDatabaseSessionPrepareUnitRestoreParamsCodec, AdapterDatabaseSessionPrepareUnitRestoreResultsCodec, okEmpty),
    dropUserRelations: session("dropUserRelations", AdapterDatabaseSessionDropUserRelationsParamsCodec, AdapterDatabaseSessionDropUserRelationsResultsCodec, okEmpty),
    assertRestoreConstraints: session("assertRestoreConstraints", AdapterDatabaseSessionAssertRestoreConstraintsParamsCodec, AdapterDatabaseSessionAssertRestoreConstraintsResultsCodec, okEmpty)
  }
};
var CAP_KINDS = /* @__PURE__ */ new Set(["source", "destination", "progress", "cancellation"]);
function requestCapTable(caps, host, stop) {
  const resolved = /* @__PURE__ */ new Map();
  return {
    exportCap() {
      throw new InvokeError(400, "invalid_params", "request message cannot export capabilities");
    },
    importCap(index) {
      if (index === null)
        return null;
      if (resolved.has(index))
        return resolved.get(index);
      const descriptor = caps[index];
      if (!descriptor || typeof descriptor !== "object" || typeof descriptor.kind !== "string") {
        throw new InvokeError(400, "invalid_params", `capability index ${index} has no descriptor`);
      }
      if (!CAP_KINDS.has(descriptor.kind)) {
        throw new InvokeError(400, "invalid_params", `unsupported request capability kind ${descriptor.kind}`);
      }
      const value = host.importCap(descriptor, stop);
      if (value === void 0) {
        throw new InvokeError(400, "invalid_params", `unsupported request capability kind ${descriptor.kind}`);
      }
      resolved.set(index, value);
      return value;
    }
  };
}
function replyCapTable(caps) {
  return {
    exportCap(value) {
      if (typeof value !== "string" || !value) {
        throw PluginError.fromWire("internal", "reply capability is not an isolate object id");
      }
      caps.push({ kind: "adapterSession", id: value });
      return caps.length - 1;
    },
    importCap() {
      throw new InvokeError(500, "internal", "reply message cannot import capabilities");
    }
  };
}
function wireError(err) {
  const e = err;
  const code = e && typeof e === "object" ? typeof e.wireCode === "string" ? e.wireCode : typeof e.code === "string" ? e.code : "internal" : "internal";
  const message = err instanceof Error ? err.message : typeof err === "string" ? err : String(err);
  return { code, message };
}
function resolveStub(entry, host) {
  if (entry.stub === "author") {
    const author = host.author();
    if (!author)
      throw new InvokeError(500, "unavailable", "PLUGIN binding missing");
    return author;
  }
  const stub = host.named(entry.stub);
  if (!stub)
    throw new InvokeError(404, "unsupported", `${entry.stub} entrypoint not exported`);
  return stub;
}
async function dispatchInvoke(iface, method, context, caps, target, body, host) {
  const table = Object.hasOwn(METHODS, iface) ? METHODS[iface] : void 0;
  const entry = table && Object.hasOwn(table, method) ? table[method] : void 0;
  if (!entry) {
    throw new InvokeError(400, "invalid_params", `unknown /invoke method ${iface}.${method}`);
  }
  const stop = new AbortController();
  let params;
  try {
    params = decodeMessage(entry.params, body, requestCapTable(caps, host, stop.signal));
  } catch (err) {
    stop.abort();
    if (err instanceof InvokeError)
      throw err;
    throw new InvokeError(400, "invalid_params", `malformed ${iface}.${method} params: ${wireError(err).message}`);
  }
  let reply;
  try {
    const stub = resolveStub(entry, host);
    const cx = { host, ctx: context, target, stop };
    try {
      reply = entry.ok(await entry.call(stub, params, cx));
    } catch (err) {
      if (err instanceof InvokeError)
        throw err;
      reply = { kind: "err", value: wireError(err) };
    }
  } finally {
    stop.abort();
  }
  const replyCaps = [];
  const bytes = encodeMessage(entry.results, { result: reply }, replyCapTable(replyCaps));
  return { body: bytes, caps: replyCaps };
}

// dist/plugin-migrations.js
var utf8 = new TextEncoder();
function utf8Bytes(value) {
  return utf8.encode(value).byteLength;
}
function opSql(op) {
  if (op && typeof op === "object") {
    if ("schema" in op && typeof op.schema === "string") {
      return op.schema;
    }
    if ("data" in op && typeof op.data === "string") {
      return op.data;
    }
  }
  return "";
}
function requirePluginMigrationRegistration(migrations) {
  if (!Array.isArray(migrations)) {
    return [];
  }
  if (migrations.length > MAX_LIST_PAGE) {
    throw migrationTooLarge(`plugin migration count ${migrations.length} exceeds maxListPage (${MAX_LIST_PAGE})`);
  }
  let total = 0;
  let totalOps = 0;
  for (const migration of migrations) {
    const ops = Array.isArray(migration.operations) ? migration.operations : [];
    if (ops.length > MAX_PLUGIN_MIGRATION_OPS) {
      throw migrationTooLarge(`plugin migration \`${migration.id}\` has ${ops.length} operations; exceeds maxPluginMigrationOps (${MAX_PLUGIN_MIGRATION_OPS})`);
    }
    totalOps += ops.length;
    if (totalOps > MAX_PLUGIN_MIGRATION_TOTAL_OPS) {
      throw migrationTooLarge(`plugin migration registration has ${totalOps} operations; exceeds maxPluginMigrationTotalOps (${MAX_PLUGIN_MIGRATION_TOTAL_OPS})`);
    }
    total += utf8Bytes(String(migration.id ?? ""));
    if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
      throw migrationTooLarge(`plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`);
    }
    for (const op of ops) {
      const n = utf8Bytes(opSql(op));
      if (n > MAX_SCALAR_BYTES) {
        throw migrationTooLarge(`plugin migration \`${migration.id}\` SQL is ${n} bytes; exceeds maxScalarBytes (${MAX_SCALAR_BYTES})`);
      }
      total += n;
      if (total > MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES) {
        throw migrationTooLarge(`plugin migration registration is ${total} bytes; exceeds maxPluginMigrationRegistrationBytes (${MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES})`);
      }
    }
  }
  return migrations;
}
function migrationTooLarge(message) {
  const err = new Error(message);
  err.code = "payload_too_large";
  err.wireCode = "payload_too_large";
  return err;
}

// dist/plugin.js
function unsupported(method) {
  return PluginError.fromWire("unsupported", `${method} not implemented`);
}
function utf8Bytes2(value) {
  return new TextEncoder().encode(String(value ?? "")).byteLength;
}
function errorMessage(err) {
  return err instanceof Error ? err.message : String(err);
}
function schemaMigrationOp(sql) {
  return { schema: sql };
}
function dataMigrationOp(sql) {
  return { data: sql };
}
var Destination = class extends RpcTarget {
  /**
   * Metadata without a body; `null` when the key is missing.
   *
   * @param _key - Object key.
   * @returns Metadata or `null` when the key is missing.
   */
  head(_key) {
    return Promise.reject(unsupported("head"));
  }
  /**
   * One page of keys under `options.prefix`.
   *
   * @param _options - Prefix, cursor, and limit.
   * @returns One page of object keys.
   */
  list(_options) {
    return Promise.reject(unsupported("list"));
  }
  /**
   * Streamed read. The body is a transferred stream, not a scalar.
   *
   * @param _key - Object key.
   * @param _options - Optional byte range.
   * @returns Metadata plus a transferred body stream.
   */
  get(_key, _options) {
    return Promise.reject(unsupported("get"));
  }
  /**
   * Streamed write. `body` ownership is transferred to the destination.
   *
   * @param _key - Object key.
   * @param _body - Byte stream.
   * @param _options - Optional content type / length.
   * @returns Bytes written and optional etag / sha256.
   */
  put(_key, _body, _options) {
    return Promise.reject(unsupported("put"));
  }
  /**
   * Server-side copy when the backend supports it.
   *
   * @param _from - Source key.
   * @param _to - Destination key.
   * @returns Bytes copied.
   */
  copy(_from, _to) {
    return Promise.reject(unsupported("copy"));
  }
  /**
   * Delete a key (no-op if missing).
   *
   * @param _key - Object key.
   * @returns Resolves when the delete is complete.
   */
  delete(_key) {
    return Promise.reject(unsupported("delete"));
  }
  /**
   * Finalize a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Idempotency / commit token.
   * @returns Published object metadata.
   */
  commit(_key, _commitToken) {
    return Promise.reject(unsupported("commit"));
  }
  /**
   * Abort a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Staging token to discard.
   * @returns Rejects with typed `unsupported` unless overridden.
   */
  abortStage(_key, _commitToken) {
    return Promise.reject(unsupported("abortStage"));
  }
};
var Source = class extends RpcTarget {
  /**
   * Opens `key` for streamed reading.
   *
   * @param _key - Object key.
   * @returns Metadata plus a transferred body stream.
   */
  open(_key) {
    return Promise.reject(unsupported("open"));
  }
};
var ProgressSink = class extends RpcTarget {
  /**
   * Reports `percent` in `0..=100` and an operator-facing `message`.
   *
   * @param _percent - Completion percent.
   * @param _message - Operator-facing status.
   * @returns Resolves when the host records progress.
   */
  report(_percent, _message) {
    return Promise.reject(unsupported("report"));
  }
};
var AdapterDatabaseSession = class extends RpcTarget {
  /**
   * Typed SQL-contract advertisement.
   *
   * @returns Guest `DbCapabilities`.
   */
  capabilities() {
    return Promise.reject(unsupported("capabilities"));
  }
  /**
   * Typed atomic batch (`ExecuteRequest` → `ExecuteReply`).
   *
   * @param _request - Cap'n `ExecuteRequest` (structured Workers RPC object).
   * @returns `ExecuteReply`.
   */
  execute(_request) {
    return Promise.reject(unsupported("execute"));
  }
  /**
   * Close the adapter session.
   *
   * @returns Resolves when the session is closed.
   */
  close() {
    return Promise.resolve();
  }
  /**
   * Bootstrap-only diagnostic metadata (`DbBootstrap`); optional.
   *
   * @returns Engine identity for diagnostics.
   */
  bootstrap() {
    return Promise.reject(unsupported("bootstrap"));
  }
  /**
   * Identity high-water marks for a logical export; optional.
   *
   * @returns One row per identity column.
   */
  exportIdentity() {
    return Promise.reject(unsupported("exportIdentity"));
  }
  /**
   * Restore identity high-water marks after a logical import; optional.
   *
   * @param _rows - Rows from an earlier {@link AdapterDatabaseSession.exportIdentity}.
   * @returns Resolves when the marks are applied.
   */
  importIdentity(_rows) {
    return Promise.reject(unsupported("importIdentity"));
  }
  /**
   * Names of user relations the host may drop before a restore; optional.
   *
   * @returns Relation names.
   */
  listUserRelations() {
    return Promise.reject(unsupported("listUserRelations"));
  }
  /**
   * Prepare the engine for a unit restore (disable constraints, …); optional.
   *
   * @returns Resolves when the engine is ready for the restore.
   */
  prepareUnitRestore() {
    return Promise.reject(unsupported("prepareUnitRestore"));
  }
  /**
   * Drop the named user relations; optional.
   *
   * @param _names - Relation names from {@link AdapterDatabaseSession.listUserRelations}.
   * @returns Resolves when the relations are gone.
   */
  dropUserRelations(_names) {
    return Promise.reject(unsupported("dropUserRelations"));
  }
  /**
   * Re-check constraints after a unit restore; optional.
   *
   * @returns Resolves when every constraint holds.
   */
  assertRestoreConstraints() {
    return Promise.reject(unsupported("assertRestoreConstraints"));
  }
};
function decodeExtensibleConfig(cfg) {
  if (cfg == null)
    return {};
  if (typeof cfg !== "object")
    return {};
  const record = cfg;
  const payload = record.payload;
  let text = "";
  if (payload instanceof Uint8Array) {
    text = new TextDecoder().decode(payload);
  } else if (payload instanceof ArrayBuffer) {
    text = new TextDecoder().decode(new Uint8Array(payload));
  } else if (typeof payload === "string") {
    text = payload;
  } else if (payload && typeof payload === "object" && !("schemaVersion" in record)) {
    return payload;
  }
  const mediaType = String(record.mediaType ?? "");
  if (!text.trim())
    return {};
  if (mediaType === "" || /json/i.test(mediaType)) {
    try {
      const parsed = JSON.parse(text);
      return parsed === null || typeof parsed !== "object" ? {} : parsed;
    } catch {
      return {};
    }
  }
  return { mediaType, text };
}
function jsonPayload(value) {
  return {
    schemaVersion: 1,
    mediaType: "application/json",
    payload: new TextEncoder().encode(JSON.stringify(value ?? null))
  };
}
function cliArgs(params) {
  const out = {};
  for (const arg of params?.args ?? []) {
    if (arg && typeof arg.name === "string")
      out[arg.name] = String(arg.value ?? "");
  }
  return out;
}
function invocationEnv(rawEnv, context) {
  const merged = { ...rawEnv ?? {} };
  if (context && typeof context === "object") {
    if (context.config !== void 0)
      merged.CONFIG = decodeExtensibleConfig(context.config);
    if (context.secrets !== void 0)
      merged.SECRETS = decodeExtensibleConfig(context.secrets);
    if (context.storage)
      merged.WORK_FS = context.storage;
    if (context.events)
      merged.EVENTS = context.events;
    if (Array.isArray(context.databases)) {
      for (const entry of context.databases) {
        if (!entry || typeof entry.name !== "string" || !entry.name)
          continue;
        if (entry.database) {
          merged[entry.name] = entry.database;
        } else if (entry.transport) {
          const transport = entry.transport;
          merged[entry.name] = createDatabaseBinding({ execute: (request) => transport.execute(request) });
        }
      }
    }
  }
  return Object.freeze(merged);
}
function applyInvocationEnv(instance, context) {
  const target = instance;
  const merged = invocationEnv(target.env, context);
  try {
    target.env = merged;
  } catch {
    Object.defineProperty(instance, "env", { value: merged, configurable: true });
  }
}
function invocationOf(context) {
  const inv = context && typeof context === "object" ? context.invocation ?? {} : {};
  return Object.freeze({
    id: String(inv.id ?? ""),
    accountId: String(inv.accountId ?? ""),
    deadlineUnixMs: Number(inv.deadlineUnixMs ?? 0) || 0,
    correlationId: String(inv.correlationId ?? ""),
    causationId: String(inv.causationId ?? "")
  });
}
function unixMs(value) {
  if (value instanceof Date)
    return value.getTime();
  const n = Number(value ?? 0);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 0;
}
function checkpointText(checkpoint) {
  if (checkpoint === void 0 || checkpoint === null)
    return "";
  const text = typeof checkpoint === "string" ? checkpoint : JSON.stringify(checkpoint);
  if (utf8Bytes2(text) > MAX_CHECKPOINT_BYTES) {
    throw PluginError.fromWire("payload_too_large", `checkpoint is ${utf8Bytes2(text)} bytes; exceeds maxCheckpointBytes (${MAX_CHECKPOINT_BYTES})`);
  }
  return text;
}
var EventMessage = class {
  #result = null;
  /** Outbox event id. */
  id;
  /** Event type (`book_acquired`, …). */
  type;
  /** Schema version of {@link EventMessage.body}. */
  schemaVersion;
  /** When the producer observed the fact. */
  timestamp;
  /** Delivery counter, starting at 1. */
  attempts;
  /** Account scope; empty for host-wide events. */
  accountId;
  /** Producer plugin id; empty when unknown. */
  source;
  /** Trace correlation id. */
  correlationId;
  /** Id of the command or event that caused this one. */
  causationId;
  /** Consumer-side idempotency key; stable across redeliveries. */
  deduplicationKey;
  /** Encoded payload bytes. */
  body;
  /** Resume ordinal; distinct from `attempts`. */
  invocationSequence;
  /** True when this delivery resumes a prior `suspend()`. */
  resumePending;
  /** Checkpoint from the prior `suspend()`, or `null`. */
  checkpoint;
  /** Raw wire envelope. */
  raw;
  constructor(event) {
    const e = event && typeof event === "object" ? event : {};
    this.id = String(e.eventId ?? "");
    this.type = String(e.eventType ?? "");
    this.schemaVersion = Number(e.schemaVersion ?? 0) || 0;
    this.timestamp = new Date(Number(e.occurredAtUnixMs ?? 0) || 0);
    this.attempts = Number(e.deliveryAttempt ?? 1) || 1;
    this.accountId = String(e.accountId ?? "");
    this.source = String(e.source ?? "");
    this.correlationId = String(e.correlationId ?? "");
    this.causationId = String(e.causationId ?? "");
    this.deduplicationKey = String(e.deduplicationKey ?? "");
    const payload = e.payload;
    this.body = payload instanceof Uint8Array ? payload : payload instanceof ArrayBuffer ? new Uint8Array(payload) : new Uint8Array();
    this.invocationSequence = Number(e.invocationSequence ?? 0) || 0;
    this.resumePending = Boolean(e.resumePending);
    const checkpointJson = String(e.checkpointJson ?? "");
    this.checkpoint = checkpointJson ? Object.freeze({
      json: checkpointJson,
      schemaVersion: Number(e.checkpointSchemaVersion ?? 0) || 0
    }) : null;
    this.raw = e;
  }
  /**
   * Decode {@link EventMessage.body} as JSON.
   *
   * @returns Parsed payload (`{}` for an empty body).
   */
  json() {
    if (this.body.byteLength === 0)
      return {};
    return JSON.parse(new TextDecoder().decode(this.body));
  }
  /**
   * Recorded outcome.
   *
   * @returns The outcome, or `null` while undecided.
   */
  get result() {
    return this.#result;
  }
  #record(result) {
    if (this.#result === null)
      this.#result = Object.freeze(result);
  }
  /** Mark handled; the host marks the event delivered. */
  ack() {
    this.#record({ kind: "ack" });
  }
  /**
   * Redeliver later: at `retryAt`, after `delaySeconds`, or — when both are
   * omitted — whenever the host's backoff chooses.
   *
   * @param options - Redelivery timing and reason.
   */
  retry(options) {
    const explicit = unixMs(options?.retryAt);
    const delay = Number(options?.delaySeconds ?? 0) || 0;
    this.#record({
      kind: "retry",
      retryAtUnixMs: explicit || (delay > 0 ? Date.now() + Math.floor(delay * 1e3) : 0),
      reason: String(options?.reason ?? "")
    });
  }
  /**
   * Stop delivering; the host records `reason`.
   *
   * @param reason - Short operator-facing reason.
   */
  reject(reason) {
    this.#record({ kind: "reject", reason: String(reason ?? "") });
  }
  /**
   * Park for operator review.
   *
   * @param reason - Short operator-facing reason.
   */
  deadLetter(reason) {
    this.#record({ kind: "deadLetter", reason: String(reason ?? "") });
  }
  /**
   * Release with a bounded checkpoint; the host redelivers at `wakeAt` or
   * when a `wakeOnEventType` event arrives.
   *
   * @param options - Checkpoint and wake conditions.
   */
  suspend(options) {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpointJson: checkpointText(opts.checkpoint),
      checkpointSchemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
      wakeAtUnixMs: unixMs(opts.wakeAt),
      wakeOnEventType: String(opts.wakeOnEventType ?? ""),
      wakeOnFilterJson: opts.wakeOnFilter === void 0 || opts.wakeOnFilter === null ? "" : typeof opts.wakeOnFilter === "string" ? opts.wakeOnFilter : JSON.stringify(opts.wakeOnFilter)
    });
  }
};
var EventBatch = class {
  /** Messages in delivery order. */
  messages;
  /** Invocation identity of this delivery. */
  invocation;
  constructor(events, invocation) {
    this.messages = Object.freeze((Array.isArray(events) ? events : []).map((event) => new EventMessage(event)));
    this.invocation = invocation ?? invocationOf(void 0);
  }
  /**
   * Event type shared by the batch.
   *
   * @returns The shared type, or `""` when mixed or empty.
   */
  get type() {
    const first = this.messages[0]?.type ?? "";
    return this.messages.every((m) => m.type === first) ? first : "";
  }
  /** Ack every message. */
  ackAll() {
    for (const m of this.messages)
      m.ack();
  }
  /**
   * Retry every message.
   *
   * @param options - Redelivery timing and reason.
   */
  retryAll(options) {
    for (const m of this.messages)
      m.retry(options);
  }
};
function eventBatchResults(batch, failed) {
  return batch.messages.map((m) => {
    if (m.result)
      return m.result;
    if (failed !== void 0) {
      return { kind: "retry", retryAtUnixMs: 0, reason: errorMessage(failed) };
    }
    return { kind: "ack" };
  });
}
var JobController = class {
  #result = null;
  #progress;
  #abort;
  /** Full durable envelope as delivered. */
  invocation;
  /** Unique id of this invocation attempt. */
  id;
  /** Command type the handler dispatches on. */
  type;
  /** Command payload JSON text. */
  payloadJson;
  /** Schema version of {@link JobController.payloadJson}. */
  payloadSchemaVersion;
  /** Caller idempotency key. */
  idempotencyKey;
  /** Failure retry counter, starting at 1. */
  attempt;
  /** Deadline hint (Unix ms); the host fence is authoritative. */
  deadlineUnixMs;
  /** Trace correlation id. */
  correlationId;
  /** Id of the event or command that caused this job. */
  causationId;
  /** Resume ordinal. */
  invocationSequence;
  /** Step id within a multi-step command; empty when single-step. */
  stepId;
  /** Checkpoint from the prior `suspend()`, or `null`. */
  checkpoint;
  /** Granted byte source, when the job has input. */
  input;
  /** Granted object store, when the job has output. */
  output;
  /** Aborts when the host cancels the invocation. */
  signal;
  constructor(invocation, granted) {
    const inv = invocation && typeof invocation === "object" ? invocation : {};
    this.invocation = Object.freeze({ ...inv });
    this.id = String(inv.invocationId ?? "");
    this.type = String(inv.commandType ?? "");
    this.payloadJson = String(inv.payloadJson ?? "");
    this.payloadSchemaVersion = Number(inv.payloadSchemaVersion ?? 0) || 0;
    this.idempotencyKey = String(inv.idempotencyKey ?? "");
    this.attempt = Number(inv.attempt ?? 1) || 1;
    this.deadlineUnixMs = Number(inv.deadlineUnixMs ?? 0) || 0;
    this.correlationId = String(inv.correlationId ?? "");
    this.causationId = String(inv.causationId ?? "");
    this.invocationSequence = Number(inv.invocationSequence ?? 0) || 0;
    this.stepId = String(inv.stepId ?? "");
    const checkpointJson = String(inv.checkpointJson ?? "");
    this.checkpoint = checkpointJson ? Object.freeze({
      json: checkpointJson,
      schemaVersion: Number(inv.checkpointSchemaVersion ?? 0) || 0
    }) : null;
    this.input = granted?.input ?? null;
    this.output = granted?.output ?? null;
    this.#progress = granted?.progress ?? null;
    this.#abort = new AbortController();
    this.signal = this.#abort.signal;
    const cancel = granted?.cancel;
    if (cancel && typeof cancel.wait === "function") {
      Promise.resolve().then(() => cancel.wait()).then(() => this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host")), () => {
      });
    }
  }
  /**
   * Decode {@link JobController.payloadJson}.
   *
   * @returns Parsed payload (`{}` when empty).
   */
  json() {
    return this.payloadJson ? JSON.parse(this.payloadJson) : {};
  }
  /**
   * Report `percent` in `0..=100` with an operator-facing `message`.
   *
   * @param percent - Completion percent.
   * @param message - Operator-facing status.
   * @returns Resolves when the host records progress.
   */
  async progress(percent, message) {
    if (!this.#progress)
      return;
    await this.#progress.report(Number(percent) || 0, String(message ?? ""));
  }
  /**
   * Recorded terminal outcome.
   *
   * @returns The outcome, or `null` while the job is still running.
   */
  get result() {
    return this.#result;
  }
  #record(result) {
    if (this.#result === null)
      this.#result = Object.freeze(result);
  }
  /**
   * Release with a bounded checkpoint; the host resumes at `wakeAt`.
   *
   * @param options - Checkpoint and wake time.
   */
  suspend(options) {
    const opts = options ?? {};
    this.#record({
      kind: "suspended",
      checkpoint: {
        schemaVersion: Number(opts.checkpointSchemaVersion ?? 1) || 1,
        json: checkpointText(opts.checkpoint)
      },
      wakeAtUnixMs: unixMs(opts.wakeAt)
    });
  }
  /**
   * Give up this attempt and let the host retry at `retryAt` (or its default).
   *
   * @param options - Retry time and reason.
   */
  retryLater(options) {
    const opts = options ?? {};
    this.#record({
      kind: "retryable",
      message: String(opts.reason ?? ""),
      retryAfterUnixMs: unixMs(opts.retryAt)
    });
  }
  /** Host-side cancellation observed (adapter-internal). */
  cancel() {
    this.#abort.abort(PluginError.fromWire("cancelled", "job cancelled by host"));
  }
};
function jobOutcomeFor(job, returned, failed) {
  if (job.result)
    return job.result;
  if (failed !== void 0) {
    const code = failed && typeof failed === "object" ? failed.wireCode ?? failed.code : void 0;
    if (job.signal.aborted || code === "cancelled") {
      return { kind: "cancelled", message: errorMessage(failed) };
    }
    if (code === "unavailable" || code === "deadline_exceeded") {
      return { kind: "retryable", message: errorMessage(failed), retryAfterUnixMs: 0 };
    }
    return { kind: "rejected", message: errorMessage(failed) };
  }
  const extra = returned && typeof returned === "object" ? returned : {};
  return {
    kind: "completed",
    message: String(extra.message ?? ""),
    bytesCopied: Number(extra.bytesCopied ?? 0) || 0
  };
}
var BookclerkEntrypoint = class extends WorkerEntrypoint {
  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * @param _request - Incoming HTTP request when the entrypoint is fetch-facing.
   * @returns Always a 404 empty response.
   */
  async fetch(_request) {
    return new Response(null, { status: 404 });
  }
  /**
   * Adapter dispatch for `describe()`.
   *
   * @returns Author refinement or `null` when not implemented.
   * @internal
   */
  async bookclerkDescribe() {
    if (typeof this.describe !== "function")
      return null;
    return await this.describe();
  }
  /**
   * Adapter dispatch for the `event(batch)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wireBatch - Wire `EventBatch`.
   * @returns One outcome per event, in order.
   * @internal
   */
  async bookclerkEvent(context, wireBatch) {
    if (typeof this.event !== "function") {
      throw unsupported("event");
    }
    applyInvocationEnv(this, context);
    const events = Array.isArray(wireBatch?.events) ? wireBatch.events : [];
    const batch = new EventBatch(events, invocationOf(context));
    try {
      await this.event(batch);
      return eventBatchResults(batch, void 0);
    } catch (err) {
      return eventBatchResults(batch, err ?? new Error("event handler failed"));
    }
  }
  /**
   * Adapter dispatch for the `job(controller)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param invocation - Durable command envelope.
   * @param granted - Granted input / output / progress / cancel stubs.
   * @returns Wire job outcome.
   * @internal
   */
  async bookclerkJob(context, invocation, granted) {
    if (typeof this.job !== "function") {
      throw unsupported("job");
    }
    applyInvocationEnv(this, context);
    const job = new JobController(invocation, granted);
    try {
      const returned = await this.job(job);
      return jobOutcomeFor(job, returned, void 0);
    } catch (err) {
      return jobOutcomeFor(job, void 0, err ?? new Error("job handler failed"));
    }
  }
  /**
   * Adapter dispatch for `databaseMigrations(binding)`.
   *
   * @param binding - Binding name from `[[databases]]`.
   * @returns Bounded registration (`[]` when not implemented).
   * @internal
   */
  async bookclerkDatabaseMigrations(binding) {
    if (typeof this.databaseMigrations !== "function")
      return [];
    const migrations = await this.databaseMigrations(String(binding ?? ""));
    return requirePluginMigrationRegistration(Array.isArray(migrations) ? migrations : []);
  }
  /**
   * Adapter dispatch for `shutdown()`.
   *
   * @returns Resolves when the author hook has run.
   * @internal
   */
  async bookclerkShutdown() {
    if (typeof this.shutdown === "function")
      await this.shutdown();
  }
};
var NamedEntrypoint = class extends WorkerEntrypoint {
  /** Methods the adapter may dispatch on this entrypoint. */
  static bookclerkMethods = [];
  /** Invocation identity of the current call (set by `bookclerkInvoke`). */
  invocation = invocationOf(void 0);
  /**
   * Rejects HTTP fetch — workerd guests are Workers-RPC only.
   *
   * @param _request - Incoming HTTP request when the entrypoint is fetch-facing.
   * @returns Always a 404 empty response.
   */
  async fetch(_request) {
    return new Response(null, { status: 404 });
  }
  /**
   * Adapter dispatch: install granted bindings, then call `method`.
   *
   * @param context - Granted bindings for this invocation.
   * @param method - Method name from the static allowlist.
   * @param args - Method arguments.
   * @returns Method result.
   * @internal
   */
  async bookclerkInvoke(context, method, ...args) {
    const allowed = this.constructor.bookclerkMethods ?? [];
    const target = this[method];
    if (!allowed.includes(method) || typeof target !== "function") {
      throw unsupported(method);
    }
    applyInvocationEnv(this, context);
    this.invocation = invocationOf(context);
    return await target.apply(this, args);
  }
};
var STOREFRONT_METHODS = Object.freeze([
  "login",
  "scan",
  "fetchTitle",
  "listAccounts",
  "loginStart",
  "loginComplete",
  "searchCatalog",
  "expandCandidates",
  "purchaseHint",
  "listDeals",
  "catalogDetail",
  "diagnose",
  "health"
]);
var StorefrontEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = STOREFRONT_METHODS;
  /**
   * Password or one-shot OAuth login. The host seals
   * `LoginResult.credentials` into `encrypted_secrets`.
   *
   * @param _params - Login parameters.
   * @returns Account identity plus opaque credentials.
   */
  login(_params) {
    return Promise.reject(unsupported("login"));
  }
  /**
   * Library scan; the host upserts `ScanSummary.books`.
   *
   * @param _params - Scan parameters (accounts + credentials).
   * @returns Books discovered plus per-account counters.
   */
  scan(_params) {
    return Promise.reject(unsupported("scan"));
  }
  /**
   * Fetches one title into `params.cacheDir` and returns plain media paths.
   *
   * @param _params - Title identifiers, credentials, fetch options.
   * @returns Plain media parts plus metadata.
   */
  fetchTitle(_params) {
    return Promise.reject(unsupported("fetchTitle"));
  }
  /**
   * Accounts the guest knows about.
   *
   * @returns Account list.
   */
  listAccounts() {
    return Promise.reject(unsupported("listAccounts"));
  }
  /**
   * Begins an interactive OAuth login.
   *
   * @param _params - Login parameters.
   * @returns Continuation for {@link StorefrontEntrypoint.loginComplete}.
   */
  loginStart(_params) {
    return Promise.reject(unsupported("loginStart"));
  }
  /**
   * Finishes a login started by {@link StorefrontEntrypoint.loginStart}.
   *
   * @param _params - Continuation plus user input.
   * @returns Account identity plus opaque credentials.
   */
  loginComplete(_params) {
    return Promise.reject(unsupported("loginComplete"));
  }
  /**
   * Free-text storefront catalog search.
   *
   * @param _params - Query parameters.
   * @returns Catalog hits.
   */
  searchCatalog(_params) {
    return Promise.reject(unsupported("searchCatalog"));
  }
  /**
   * Related-title expansion from a seed title.
   *
   * @param _params - Seed title plus limits.
   * @returns Candidate hits.
   */
  expandCandidates(_params) {
    return Promise.reject(unsupported("expandCandidates"));
  }
  /**
   * Purchase link / price hint; `null` when the title is unknown.
   *
   * @param _params - Title identifiers.
   * @returns Hint or `null`.
   */
  purchaseHint(_params) {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Current storefront deals.
   *
   * @param _params - Optional filters.
   * @returns Deal hits.
   */
  listDeals(_params) {
    return Promise.reject(unsupported("listDeals"));
  }
  /**
   * Full catalog record for one product; `null` when unknown.
   *
   * @param _params - Product identifiers.
   * @returns Catalog hit or `null`.
   */
  catalogDetail(_params) {
    return Promise.reject(unsupported("catalogDetail"));
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose() {
    return Promise.resolve([]);
  }
  /**
   * Reports whether the storefront session is usable.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
};
var STORAGE_METHODS = Object.freeze([
  "head",
  "list",
  "get",
  "put",
  "copy",
  "delete",
  "commit",
  "abortStage"
]);
var StorageEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = STORAGE_METHODS;
  /**
   * Metadata without a body; `null` when the key is missing.
   *
   * @param _key - Object key.
   * @returns Metadata or `null` when the key is missing.
   */
  head(_key) {
    return Promise.reject(unsupported("head"));
  }
  /**
   * One page of keys under `options.prefix`.
   *
   * @param _options - Prefix, cursor, and limit.
   * @returns One page of object keys.
   */
  list(_options) {
    return Promise.reject(unsupported("list"));
  }
  /**
   * Streamed read. The body is a transferred stream, not a scalar.
   *
   * @param _key - Object key.
   * @param _options - Optional byte range.
   * @returns Metadata plus a transferred body stream.
   */
  get(_key, _options) {
    return Promise.reject(unsupported("get"));
  }
  /**
   * Streamed write. `body` ownership is transferred to the destination.
   *
   * @param _key - Object key.
   * @param _body - Byte stream.
   * @param _options - Optional content type / length.
   * @returns Bytes written and optional etag / sha256.
   */
  put(_key, _body, _options) {
    return Promise.reject(unsupported("put"));
  }
  /**
   * Server-side copy when the backend supports it.
   *
   * @param _from - Source key.
   * @param _to - Destination key.
   * @returns Bytes copied.
   */
  copy(_from, _to) {
    return Promise.reject(unsupported("copy"));
  }
  /**
   * Delete a key (no-op if missing).
   *
   * @param _key - Object key.
   * @returns Resolves when the delete is complete.
   */
  delete(_key) {
    return Promise.reject(unsupported("delete"));
  }
  /**
   * Finalize a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Idempotency / commit token.
   * @returns Published object metadata.
   */
  commit(_key, _commitToken) {
    return Promise.reject(unsupported("commit"));
  }
  /**
   * Abort a destination-side staged object.
   *
   * @param _key - Object key.
   * @param _commitToken - Staging token to discard.
   * @returns Rejects with typed `unsupported` unless overridden.
   */
  abortStage(_key, _commitToken) {
    return Promise.reject(unsupported("abortStage"));
  }
};
var REMOTE_LIBRARY_METHODS = Object.freeze([
  "health",
  "diagnose",
  "start",
  "stop",
  "scanLibrary",
  "syncListening",
  "pollEvents"
]);
var RemoteLibraryEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = REMOTE_LIBRARY_METHODS;
  /**
   * Reports whether the remote library session is usable.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Operator-facing diagnostic lines.
   *
   * @returns Probe lines.
   */
  diagnose() {
    return Promise.resolve([]);
  }
  /**
   * Starts long-running work for this invocation.
   *
   * @returns Resolves when running.
   */
  start() {
    return Promise.resolve();
  }
  /**
   * Stops long-running work for this invocation.
   *
   * @returns Resolves when stopped.
   */
  stop() {
    return Promise.resolve();
  }
  /**
   * Re-syncs the remote library.
   *
   * @param _params - Scan scope.
   * @returns Resolves when the scan has been accepted.
   */
  scanLibrary(_params) {
    return Promise.reject(unsupported("scanLibrary"));
  }
  /**
   * Push / pull listening progress; the host upserts the rows.
   *
   * @returns Progress rows.
   */
  syncListening() {
    return Promise.reject(unsupported("syncListening"));
  }
  /**
   * Drains external users observed since the last poll.
   *
   * @returns Newly observed users.
   */
  pollEvents() {
    return Promise.reject(unsupported("pollEvents"));
  }
};
var ADAPTER_SESSIONS = /* @__PURE__ */ new Map();
var ADAPTER_SESSION_METHODS = /* @__PURE__ */ new Set([
  "capabilities",
  "execute",
  "close",
  "bootstrap",
  "exportIdentity",
  "importIdentity",
  "listUserRelations",
  "prepareUnitRestore",
  "dropUserRelations",
  "assertRestoreConstraints"
]);
function freshSessionId() {
  const c = globalThis.crypto;
  if (c && typeof c.randomUUID === "function")
    return c.randomUUID();
  const bytes = new Uint8Array(16);
  if (c && typeof c.getRandomValues === "function")
    c.getRandomValues(bytes);
  else
    for (let i = 0; i < bytes.length; i++)
      bytes[i] = Math.floor(Math.random() * 256);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}
var DatabaseAdapterEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["openSession"]);
  /**
   * Opens a session. Sessions cannot survive suspension.
   *
   * @returns Adapter session capability.
   */
  openSession() {
    return Promise.reject(unsupported("openSession"));
  }
  /**
   * Adapter dispatch: `openSession` → session id; `session` → call on a
   * retained session (`args` = `[id, method, ...params]`).
   *
   * @param context - Granted bindings for this invocation.
   * @param method - `openSession` or `session`.
   * @param args - Method arguments.
   * @returns Session id for `openSession`, else the session method result.
   * @internal
   */
  async bookclerkInvoke(context, method, ...args) {
    if (method === "openSession") {
      const session2 = await super.bookclerkInvoke(context, "openSession");
      if (!session2 || typeof session2 !== "object") {
        throw PluginError.fromWire("internal", "openSession returned no session");
      }
      const id = freshSessionId();
      ADAPTER_SESSIONS.set(id, session2);
      return id;
    }
    if (method === "session") {
      const [id, sessionMethod, ...params] = args;
      const session2 = typeof id === "string" ? ADAPTER_SESSIONS.get(id) : void 0;
      if (!session2) {
        throw PluginError.fromWire("invalid_params", `unknown adapter session ${String(id)}`);
      }
      const name = String(sessionMethod ?? "");
      const target = session2[name];
      if (!ADAPTER_SESSION_METHODS.has(name) || typeof target !== "function") {
        throw unsupported(name);
      }
      applyInvocationEnv(this, context);
      this.invocation = invocationOf(context);
      try {
        return await target.apply(session2, params);
      } finally {
        if (name === "close")
          ADAPTER_SESSIONS.delete(id);
      }
    }
    return super.bookclerkInvoke(context, method, ...args);
  }
};
var CliEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["describe", "invoke"]);
  /**
   * Guest CLI schema.
   *
   * @returns Declared commands (`{ commands: [] }` when the guest has no CLI).
   */
  describe() {
    return Promise.resolve({ commands: [] });
  }
  /**
   * Invokes a guest CLI command.
   *
   * @param _params - Command name plus named argument values.
   * @returns Exit code, captured output, optional structured payload.
   */
  invoke(_params) {
    return Promise.reject(unsupported("invoke"));
  }
};
var OidcEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["clients", "authenticateUser"]);
  /**
   * Plugin-provided OIDC authorization-server client templates. The host
   * materializes `oidc_clients` rows; plugins never mint tokens.
   *
   * @returns Templates (`[]` when unused).
   */
  clients() {
    return Promise.resolve([]);
  }
  /**
   * Verifies remote credentials on behalf of the host.
   *
   * @param _params - Username / password pair.
   * @returns External user identity.
   */
  authenticateUser(_params) {
    return Promise.reject(unsupported("authenticateUser"));
  }
};
var ENTRYPOINT_BINDINGS = Object.freeze({
  storefront: "PLUGIN_STOREFRONT",
  storage: "PLUGIN_STORAGE",
  databaseAdapter: "PLUGIN_DATABASE_ADAPTER",
  remoteLibrary: "PLUGIN_REMOTE_LIBRARY",
  cli: "PLUGIN_CLI",
  oidc: "PLUGIN_OIDC"
});
var ENTRYPOINT_CLASSES = Object.freeze({
  storefront: "Storefront",
  storage: "Storage",
  databaseAdapter: "DatabaseAdapter",
  remoteLibrary: "RemoteLibrary",
  cli: "Cli",
  oidc: "Oidc"
});
function exactLengthBody(body, expected) {
  if (body == null || expected == null || !Number.isFinite(expected)) {
    return body;
  }
  const reader = body.getReader();
  let n = 0;
  return new ReadableStream({
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done) {
        if (n !== expected) {
          controller.error(PluginError.fromWire("invalid_params", `content-length ${expected} got ${n}`));
          return;
        }
        controller.close();
        return;
      }
      n += value.byteLength;
      if (n > expected) {
        controller.error(PluginError.fromWire("payload_too_large", `body exceeded Content-Length ${expected}`));
        return;
      }
      controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    }
  });
}
function streamedRead(resp, key) {
  return {
    meta: {
      key: resp.headers.get("x-bookclerk-key") || key,
      size: Number(resp.headers.get("x-bookclerk-size") || "0"),
      contentType: resp.headers.get("x-bookclerk-content-type") || void 0,
      etag: resp.headers.get("x-bookclerk-etag") || void 0
    },
    body: resp.body
  };
}
var GrantedSource = class extends Source {
  #granted;
  #auth;
  #signal;
  constructor(granted, auth, signal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }
  async open(key) {
    const resp = await this.#granted.fetch(`http://granted/open?key=${encodeURIComponent(key)}`, {
      headers: this.#auth,
      signal: this.#signal
    });
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return streamedRead(resp, key);
  }
};
var GrantedDestination = class extends Destination {
  #granted;
  #auth;
  #signal;
  constructor(granted, auth, signal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }
  async put(key, body, options) {
    const headers = { ...this.#auth };
    if (options?.contentType)
      headers["content-type"] = options.contentType;
    if (options?.contentLength != null) {
      headers["content-length"] = String(options.contentLength);
    }
    const resp = await this.#granted.fetch(`http://granted/put?key=${encodeURIComponent(key)}`, {
      method: "PUT",
      headers,
      body,
      signal: this.#signal
    });
    if (!resp.ok) {
      throw PluginError.fromWire("internal", await resp.text());
    }
    return await resp.json();
  }
};
var GrantedProgress = class extends ProgressSink {
  #granted;
  #auth;
  #signal;
  constructor(granted, auth, signal) {
    super();
    this.#granted = granted;
    this.#auth = auth;
    this.#signal = signal;
  }
  async report(percent, message) {
    await this.#granted.fetch(`http://granted/progress`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify({ percent, message: message || "" }),
      signal: this.#signal
    });
  }
};
function publishPayloadBytes(payload) {
  if (payload === void 0 || payload === null)
    return new Uint8Array(0);
  if (payload instanceof Uint8Array)
    return payload;
  if (payload instanceof ArrayBuffer)
    return new Uint8Array(payload);
  if (typeof payload === "string")
    return new TextEncoder().encode(payload);
  return new TextEncoder().encode(JSON.stringify(payload));
}
var GrantedEvents = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the events grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Publish one domain event through the host outbox.
   *
   * @param event - Event type, schema version, payload, dedup key.
   * @returns Outbox row identity.
   */
  async publish(event) {
    if (!event || typeof event.eventType !== "string" || !event.eventType) {
      throw PluginError.fromWire("invalid_params", "publish requires eventType");
    }
    const payload = publishPayloadBytes(event.payload);
    if (payload.byteLength > MAX_EVENT_PAYLOAD_BYTES) {
      throw PluginError.fromWire("payload_too_large", `event payload of ${payload.byteLength} bytes exceeds ${MAX_EVENT_PAYLOAD_BYTES}`);
    }
    const wire = toBridgeJson({
      eventType: event.eventType,
      schemaVersion: Number(event.schemaVersion ?? 1) || 1,
      deduplicationKey: String(event.deduplicationKey ?? ""),
      payload,
      occurredAtUnixMs: Number(event.occurredAtUnixMs ?? 0) || 0,
      correlationId: String(event.correlationId ?? ""),
      causationId: String(event.causationId ?? "")
    });
    const resp = await this.#granted.fetch("http://granted/events/publish", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire)
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events publish HTTP ${resp.status}`);
    }
    return { eventId: String(value.eventId ?? ""), duplicate: Boolean(value.duplicate) };
  }
};
var GrantedDatabase = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the database grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Run one typed atomic batch through the host broker.
   *
   * @param request - Structured `ExecuteRequest`.
   * @returns Structured `ExecuteReply`.
   */
  async execute(request) {
    return decodeExecuteResultReply(await this.executeBytes(encodeExecuteRequest(request)));
  }
  /**
   * Same batch as Cap'n Proto bytes (Python authors encode locally).
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Unpacked `ExecuteResultReply` message.
   */
  async executeBytes(bytes) {
    const resp = await this.#granted.fetch("http://granted/db/execute", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/octet-stream" },
      body: bytes
    });
    if (!resp.ok) {
      const text = await resp.text().catch(() => "");
      throw PluginError.fromWire(resp.status === 401 ? "unauthorized" : resp.status === 413 ? "payload_too_large" : "unavailable", `database grant: ${resp.status} ${text}`.trim());
    }
    return new Uint8Array(await resp.arrayBuffer());
  }
};
var CancelWatch = class extends RpcTarget {
  #promise;
  constructor(granted, auth, stop) {
    super();
    this.#promise = new Promise((resolve) => {
      const poll = async () => {
        while (!stop.aborted) {
          const resp = await granted.fetch("http://granted/cancel", { headers: auth, signal: stop });
          if (resp.status === 200) {
            resolve();
            return;
          }
          if (resp.status !== 204)
            return;
        }
      };
      poll().catch(() => {
      });
    });
  }
  wait() {
    return this.#promise;
  }
};
function bytesToBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++)
    bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}
function base64ToBytes(text) {
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++)
    out[i] = bin.charCodeAt(i);
  return out;
}
var BRIDGE_BYTES_FIELDS = /* @__PURE__ */ new Set(["payload", "sha256", "credentials"]);
function fromBridgeJson(value) {
  if (Array.isArray(value))
    return value.map(fromBridgeJson);
  if (value === null || typeof value !== "object")
    return value;
  const out = {};
  for (const [key, inner] of Object.entries(value)) {
    if (BRIDGE_BYTES_FIELDS.has(key) && typeof inner === "string") {
      out[key] = base64ToBytes(inner);
    } else if (BRIDGE_BYTES_FIELDS.has(key) && Array.isArray(inner) && inner.every((n) => typeof n === "number")) {
      out[key] = Uint8Array.from(inner);
    } else {
      out[key] = fromBridgeJson(inner);
    }
  }
  return out;
}
function toBridgeJson(value) {
  if (value instanceof Uint8Array)
    return bytesToBase64(value);
  if (value instanceof ArrayBuffer)
    return bytesToBase64(new Uint8Array(value));
  if (Array.isArray(value))
    return value.map(toBridgeJson);
  if (value === null || typeof value !== "object")
    return value;
  const out = {};
  for (const [key, inner] of Object.entries(value)) {
    if (inner === void 0)
      continue;
    out[key] = toBridgeJson(inner);
  }
  return out;
}
function parseHeaderJson(text, name) {
  if (text === null || text === void 0 || text === "")
    return void 0;
  try {
    return JSON.parse(String(text));
  } catch (err) {
    throw new InvokeError(400, "invalid_params", `${name} header is not JSON: ${errorMessage(err)}`);
  }
}
function parseJsonBinding(value) {
  if (value == null)
    return null;
  if (typeof value === "string") {
    try {
      return JSON.parse(value);
    } catch {
      return null;
    }
  }
  return typeof value === "object" ? value : null;
}
function wrapPluginFromBinding() {
  return createInvocationAdapter();
}
function wrapPluginFromNative() {
  return createInvocationAdapter();
}
var TRIGGER_FAMILIES = Object.freeze({
  eventConsumer: "eventConsumer",
  jobRunner: "jobRunner"
});
function allowedEntrypointFamilies(capabilities, requested) {
  if (!capabilities || typeof capabilities !== "object")
    return [];
  const entrypoints = Array.isArray(capabilities.entrypoints) ? capabilities.entrypoints : [];
  const consumes = Array.isArray(capabilities.consumes) ? capabilities.consumes : [];
  const jobs = Array.isArray(capabilities.jobs) ? capabilities.jobs : [];
  const exportsStorage = entrypoints.includes("storage");
  const out = [];
  for (const name of requested) {
    if (typeof name !== "string" || out.includes(name))
      continue;
    if (name === TRIGGER_FAMILIES.eventConsumer) {
      if (consumes.length > 0)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.jobRunner) {
      if (jobs.length > 0 || exportsStorage)
        out.push(name);
    } else if (name in ENTRYPOINT_BINDINGS && entrypoints.includes(name)) {
      out.push(name);
    }
  }
  return out;
}
function mergeDescribe(base, guest) {
  const guestCli = guest.cli;
  const cli = guestCli && Array.isArray(guestCli.commands) && guestCli.commands.length > 0 ? guestCli : base.cli ?? guestCli;
  return {
    ...base,
    ...guest,
    apiVersion: base.apiVersion ?? guest.apiVersion ?? 3,
    id: base.id ?? guest.id,
    capabilities: base.capabilities ?? guest.capabilities,
    ...cli === void 0 ? {} : { cli }
  };
}
function createInvocationAdapter() {
  return class InvocationAdapter extends WorkerEntrypoint {
    #manifest() {
      return parseJsonBinding(this.env.PLUGIN_DESCRIBE) ?? {};
    }
    #author() {
      if (this.env.PLUGIN)
        return this.env.PLUGIN;
      throw PluginError.fromWire("unavailable", "PLUGIN binding missing");
    }
    #named(name) {
      const binding = ENTRYPOINT_BINDINGS[name];
      const stub = binding ? this.env[binding] : void 0;
      if (!stub) {
        throw PluginError.fromWire("unsupported", `${name} entrypoint not exported`);
      }
      return stub;
    }
    /**
     * Turn the bridge context into the author-facing one: the host's events
     * grant token becomes an `EVENTS` stub and never reaches the author.
     *
     * @param ctx - Bridge context from the launcher.
     * @returns Author-facing granted context.
     */
    #bindContext(ctx) {
      const source = ctx && typeof ctx === "object" ? ctx : {};
      const { eventsToken, databases, ...rest } = source;
      const bound = rest;
      const granted = this.env.GRANTED;
      if (typeof eventsToken === "string" && eventsToken && granted) {
        bound.events = new GrantedEvents(granted, {
          Authorization: `Bearer ${eventsToken}`
        });
      }
      if (databases && typeof databases === "object" && !Array.isArray(databases) && granted) {
        const named2 = [];
        for (const [name, token] of Object.entries(databases)) {
          if (!name || typeof token !== "string" || !token)
            continue;
          named2.push({
            name,
            transport: new GrantedDatabase(granted, { Authorization: `Bearer ${token}` })
          });
        }
        if (named2.length > 0)
          bound.databases = named2;
      }
      return bound;
    }
    async fetch(_request) {
      return new Response(null, { status: 404 });
    }
    /**
     * `PluginDescribe`: the manifest projection the launcher binds as
     * `PLUGIN_DESCRIBE`, refined by the guest's describe. Author mode calls
     * the author's optional `describe()`; native-behind-workerd passes the
     * native guest's typed describe as `native`. Identity and capabilities
     * always come from the manifest.
     *
     * @param native - Native guest describe (native-behind-workerd only).
     * @returns Wire describe.
     */
    async describe(native) {
      const base = this.#manifest();
      if (native !== void 0) {
        if (native === null || typeof native !== "object") {
          throw PluginError.fromWire("invalid_params", "native describe must be an object");
        }
        return mergeDescribe(base, native);
      }
      const author = await this.#author().bookclerkDescribe() ?? {};
      return mergeDescribe(base, author);
    }
    /**
     * `/open` policy for native-behind-workerd: validates the invocation
     * envelope and returns the requested entrypoint / trigger families the
     * manifest (`PLUGIN_DESCRIBE`) lets this plugin export. The launcher
     * nulls every family that is not returned.
     *
     * @param ctx - Open context (`invocation`, `config`, `secrets`).
     * @param requested - Family names the launcher wants to export.
     * @returns Authorized subset of `requested`.
     */
    async openInvocation(ctx, requested = []) {
      const invocation = ctx && typeof ctx === "object" ? ctx.invocation : void 0;
      if (!invocation || typeof invocation.id !== "string" || !invocation.id) {
        throw PluginError.fromWire("invalid_params", "open requires a non-empty invocation id");
      }
      const list = Array.isArray(requested) ? requested : [];
      return allowedEntrypointFamilies(this.#manifest().capabilities, list);
    }
    /**
     * Call `method` on the named entrypoint `name` with the granted context.
     *
     * @param name - Entrypoint wire name.
     * @param ctx - Granted context.
     * @param method - Method name.
     * @param args - Method arguments.
     * @returns Method result.
     */
    async invokeEntrypoint(name, ctx, method, args = []) {
      const list = Array.isArray(args) ? args : [];
      return this.#named(name).bookclerkInvoke(this.#bindContext(ctx), method, ...list);
    }
    /**
     * Stream routes (`GET /destination/get`, `PUT /destination/put`) →
     * `storage` entrypoint. Every other `Destination` method travels on
     * `POST /invoke` ({@link InvocationAdapter.invoke}).
     *
     * @param op - `get` or `put`.
     * @param ctx - Granted context.
     * @param args - Route arguments.
     * @param body - Stream body for `put`.
     * @returns Method result.
     */
    async invokeDestination(op, ctx, args = {}, body) {
      switch (op) {
        case "get":
          return this.invokeEntrypoint("storage", ctx, "get", [String(args.key ?? ""), args.options]);
        case "put": {
          if (!body) {
            throw PluginError.fromWire("invalid_params", "put missing body stream");
          }
          const options = args.options;
          const bounded = exactLengthBody(body, options?.contentLength);
          return this.invokeEntrypoint("storage", ctx, "put", [String(args.key ?? ""), bounded, options]);
        }
        default:
          throw PluginError.fromWire("unsupported", `destination.${op} is not a stream route`);
      }
    }
    /**
     * `/source/open` route → `storage.get`.
     *
     * @param ctx - Granted context.
     * @param key - Object key.
     * @returns Opened byte source result.
     */
    async invokeSourceOpen(ctx, key) {
      return this.invokeEntrypoint("storage", ctx, "get", [String(key ?? "")]);
    }
    /**
     * Resolve one request capability descriptor (`X-Bookclerk-Caps` entry)
     * into the granted-channel stub handed to the author.
     *
     * @param descriptor - Capability descriptor.
     * @param stop - Aborts when the call returns.
     * @returns Granted stub, or `undefined` for an unknown kind.
     */
    #importCap(descriptor, stop) {
      const granted = this.env.GRANTED;
      if (!granted) {
        throw new InvokeError(500, "unavailable", "GRANTED reverse channel missing");
      }
      const auth = { Authorization: `Bearer ${String(descriptor.token ?? "")}` };
      switch (descriptor.kind) {
        case "source":
          return new GrantedSource(granted, auth, stop);
        case "destination":
          return new GrantedDestination(granted, auth, stop);
        case "progress":
          return new GrantedProgress(granted, auth, stop);
        case "cancellation":
          return new CancelWatch(granted, auth, stop);
        default:
          return void 0;
      }
    }
    /**
     * `POST /invoke`: one ABI method call as Cap'n Proto bytes. The bridge
     * passes the request headers through verbatim; the reply carries the
     * `$Results` bytes plus the descriptors of capabilities it exported.
     * Transport failures (unknown method, malformed message, missing
     * binding) come back as an {@link InvokeFailure} with the HTTP status the
     * bridge should answer with — never as a thrown error, because Workers
     * RPC drops own properties of thrown errors.
     *
     * @param iface - `X-Bookclerk-Interface` (Cap'n interface name).
     * @param method - `X-Bookclerk-Method`.
     * @param contextJson - `X-Bookclerk-Context` bridge JSON, or `null`.
     * @param capsJson - `X-Bookclerk-Caps` JSON descriptor array, or `null`.
     * @param target - `X-Bookclerk-Target` isolate object id, or `null`.
     * @param body - `$Params` message bytes.
     * @returns Reply bytes plus capability descriptors, or a transport failure.
     */
    async invoke(iface, method, contextJson, capsJson, target, body) {
      const host = {
        author: () => this.env.PLUGIN,
        named: (name) => {
          const binding = ENTRYPOINT_BINDINGS[name];
          return this.env[binding];
        },
        bindContext: (ctx) => this.#bindContext(ctx),
        importCap: (descriptor, stop) => this.#importCap(descriptor, stop)
      };
      try {
        const context = parseHeaderJson(contextJson, "context");
        if (context !== void 0 && (context === null || typeof context !== "object" || Array.isArray(context))) {
          throw new InvokeError(400, "invalid_params", "context header must be a JSON object");
        }
        const caps = parseHeaderJson(capsJson, "caps") ?? [];
        if (!Array.isArray(caps)) {
          throw new InvokeError(400, "invalid_params", "caps header must be a JSON array");
        }
        const ctx = context === void 0 ? void 0 : fromBridgeJson(context);
        return await dispatchInvoke(String(iface ?? ""), String(method ?? ""), ctx, caps, typeof target === "string" && target ? target : null, body instanceof Uint8Array ? body : new Uint8Array(body ?? []), host);
      } catch (err) {
        if (err instanceof InvokeError) {
          return { status: err.status, error: { code: err.wireCode, message: err.message } };
        }
        return { status: 500, error: { code: "internal", message: errorMessage(err) } };
      }
    }
    /**
     * `/shutdown` route → default entrypoint `shutdown`. Without an author
     * `PLUGIN` binding (native-behind-workerd) there is nothing to run here:
     * the launcher shuts the native guest down over Cap'n Proto.
     *
     * @returns Resolves when the author hook has run.
     */
    async shutdown() {
      if (!this.env.PLUGIN)
        return;
      await this.#author().bookclerkShutdown();
    }
  };
}
export {
  AdapterDatabaseSession,
  BookclerkEntrypoint,
  CliEntrypoint,
  DatabaseAdapterEntrypoint,
  Destination,
  ENTRYPOINT_BINDINGS,
  ENTRYPOINT_CLASSES,
  EventBatch,
  EventMessage,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STORAGE_COPY,
  FEATURE_STREAMS,
  InvokeError,
  JobController,
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  NamedEntrypoint,
  OidcEntrypoint,
  PRODUCT_API_VERSION,
  PluginError,
  ProgressSink,
  RemoteLibraryEntrypoint,
  Source,
  StorageEntrypoint,
  StorefrontEntrypoint,
  TRIGGER_FAMILIES,
  allowedEntrypointFamilies,
  canonicalExecuteRequestHash,
  cliArgs,
  createDatabaseBinding,
  dataMigrationOp,
  decodeDbValue,
  decodeExecuteRequest,
  decodeExecuteResultReply,
  decodeExtensibleConfig,
  dispatchInvoke,
  encodeDbValue,
  encodeExecuteRequest,
  encodeExecuteResultReply,
  eventBatchResults,
  executeReplyToD1Results,
  fromBridgeJson,
  invocationOf,
  jobOutcomeFor,
  jsonPayload,
  parseDbValue,
  requirePluginMigrationRegistration,
  schemaMigrationOp,
  statementResultToD1Result,
  toBridgeJson,
  wrapPluginFromBinding,
  wrapPluginFromNative
};
