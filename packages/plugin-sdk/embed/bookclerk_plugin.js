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
var MAX_BATCH_KEYS = 256;
var MAX_CHANGES_PAGE = 256;
var MAX_CHANGES_CURSOR_BYTES = 4096;
var MAX_CHECKPOINT_BYTES = 65536;
var MAX_SCAN_PAGE_BOOKS = 500;
var MAX_SCAN_CURSOR_BYTES = 4096;
var MAX_EVENT_PAYLOAD_BYTES = 65536;
var MAX_KV_VALUE_BYTES = 1048576;
var MAX_KV_KEY_BYTES = 512;
var MAX_SOCKET_FRAME_BYTES = 262144;
var MAX_SOCKET_ATTACHMENT_BYTES = 2048;
var MAX_ORIGIN_HEADER_BYTES = 65536;
var MAX_ORIGIN_TARGET_BYTES = 8192;
var MAX_WEBHOOK_PULL_BATCH = 100;
var MAX_WEBHOOK_LEASE_ID_BYTES = 128;
var MAX_WEBHOOK_VISIBILITY_MS = 432e5;
var MAX_WEBHOOK_NAME_BYTES = 32;
var MAX_KV_METADATA_BYTES = 1024;
var MAX_PLUGIN_MIGRATION_OPS = 256;
var MAX_PLUGIN_MIGRATION_TOTAL_OPS = 2048;
var MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES = 262144;
var FEATURE_SCALAR_LIMITS = "rpc.scalarLimits";
var FEATURE_STREAMS = "rpc.streams";
var FEATURE_STORAGE_COPY = "storage.copy";
var FEATURE_STORAGE_MOVE = "storage.move";
var FEATURE_STORAGE_MOVE_MANY = "storage.moveMany";
var FEATURE_STORAGE_DELETE_MANY = "storage.deleteMany";
var FEATURE_STORAGE_HEAD_MANY = "storage.headMany";
var FEATURE_STORAGE_CONDITIONAL_WRITE = "storage.conditionalWrite";
var FEATURE_STORAGE_CHANGES = "storage.changes";
var OBJECT_CHANGE_KINDS = ["changed", "upserted", "removed"];
var PLUGIN_ERROR_CODES = ["invalid_params", "unauthorized", "forbidden", "not_found", "unavailable", "unsupported", "internal", "payload_too_large", "deadline_exceeded", "invalid_cursor", "cancelled", "conflict", "limit_exceeded", "rate_limited", "network_not_granted"];
var CLI_ARG_KINDS = ["string", "bool", "int", "path"];
var WORK_KINDS = ["unspecified", "book", "podcast"];
var EXPRESSION_KINDS = ["unspecified", "text", "narration", "dramatisation", "graphic", "originalAudio"];
var MEDIA_KINDS = ["unspecified", "audio", "ebook", "print"];
var IDENTIFIER_SCHEMES = ["isbn", "asin", "openLibraryEdition", "openLibraryWork"];
var CREDIT_ROLES = ["author", "narrator", "translator", "editor", "illustrator", "publisher", "host", "cast", "adaptor"];
var CREDIT_LEVELS = ["roleDefault", "work", "expression", "edition"];
var FETCHED_FILE_ROLES = ["unspecified", "part", "whole", "cover"];
var CHAPTER_KINDS = ["unknown", "chapters", "tracks"];
var CATALOG_SORTS = ["relevance", "popularity", "rating", "title", "author"];
var CATALOG_FIELDS = ["any", "author", "narrator", "series", "genre"];
var PRICE_LOOKUPS = ["unspecified", "product", "isbn", "asin", "title"];
var ABRIDGEMENTS = ["unknown", "unabridged", "abridged"];
var EXPLICITNESSS = ["unknown", "clean", "explicit"];
var OFFERED_CODECS = ["unknown", "aac", "alac", "flac", "mp3", "opus", "vorbis", "eac3", "ac4"];
var OFFERED_AAC_PROFILES = ["unknown", "lc", "he", "heV2", "xhe"];
var METADATA_FIELDS = ["title", "subtitle", "description", "publisher", "publishedAt", "language", "genres", "tags", "abridgement", "expressionKind", "identifiers", "credits", "series", "chapters", "duration", "explicitness"];
var NOTIFICATION_SEVERITYS = ["info", "warning", "critical"];
var NOTIFICATION_ALERT_ACTIONS = ["none", "trigger", "resolve"];
var NOTIFY_DESTINATION_KINDS = ["channel", "personal"];
var AI_TASKS = ["textEmbeddings"];
var AI_TEXT_ROLES = ["document", "query"];
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
  /** Largest segment, pointer list or composite list, in words. */
  maxWords;
  /**
   * Words the reader may still visit. Every struct and list reached is
   * charged its size, so a message whose pointers alias one large value
   * many times over cannot materialize more than the budget, as the Cap'n
   * Proto traversal limit does.
   */
  remaining;
  /**
   * @param bytes - Unpacked single-segment message.
   * @param maxWords - Traversal budget in words; the default suits control
   *   messages, a streamed result frame passes the budget its stream
   *   negotiated.
   */
  constructor(bytes, maxWords = MAX_TRAVERSAL_WORDS) {
    this.maxWords = maxWords;
    this.remaining = maxWords;
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
    if (size0 > this.maxWords) {
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
    this.visit(target, dw + pw);
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
  /**
   * Checks that `nWords` from `word` lie in the segment and charges them to
   * the traversal budget.
   *
   * @param word - First word of the struct or list reached.
   * @param nWords - Its size in words.
   * @throws When the range leaves the segment or exhausts the budget.
   */
  visit(word, nWords) {
    this.checkRange(word, nWords);
    this.charge(nWords);
  }
  /**
   * Charges `nWords` to the traversal budget without a range check, for
   * elements that occupy no words of their own.
   *
   * @param nWords - Words to charge.
   * @throws `Error` when the budget is exhausted.
   */
  charge(nWords) {
    this.remaining -= nWords;
    if (this.remaining < 0) {
      throw new Error("Cap'n message exceeds traversal budget");
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
    this.visit(lp.target, nWords);
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
    if (lp.length > this.maxWords) {
      throw new Error("pointer list exceeds traversal budget");
    }
    this.visit(lp.target, lp.length);
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
    this.visit(lp.target, Math.ceil(lp.length * byteWidth / WORD));
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
    this.visit(lp.target, Math.ceil(lp.length / 64));
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
    if (lp === null) {
      return [];
    }
    const { tagWord, count, dw, pw } = this.compositeTag(lp);
    const elemWords = dw + pw;
    this.checkRange(tagWord, 1 + count * elemWords);
    this.charge(1 + Math.max(count * elemWords, count));
    const out = [];
    for (let i = 0; i < count; i++) {
      out.push(new StructReader(this, tagWord + 1 + i * elemWords, dw, pw));
    }
    return out;
  }
  /**
   * Element count of a composite list without visiting its elements, so a
   * caller can refuse a list that is too long before any element decodes.
   *
   * @param ptrWord - Word holding the list pointer.
   * @returns The number of elements the list's tag announces, `0` for a
   *   null or empty list.
   * @throws `Error` when the pointer is not a composite list pointer or the
   *   count exceeds the list's payload.
   */
  readStructListLength(ptrWord) {
    const lp = this.listPointer(ptrWord, 7);
    if (lp === null) {
      return 0;
    }
    return this.compositeTag(lp).count;
  }
  /**
   * The tag word of a composite list: its element count and element shape.
   *
   * A composite list of zero words still carries its tag (zero-width
   * elements, or no elements), so the tag is read whenever the pointer is
   * not null.
   *
   * @param lp - A composite (element size 7) list pointer.
   * @returns The tag's word, element count, data words and pointer words.
   * @throws `Error` when the count exceeds the list's payload or the
   *   traversal budget.
   */
  compositeTag(lp) {
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
    if (count > this.maxWords) {
      throw new Error("composite list count exceeds traversal budget");
    }
    return { tagWord, count, dw, pw };
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
  /**
   * Element count of the composite list at `pointerIndex`, read from its tag
   * alone (see {@link CapnpReader.readStructListLength}).
   *
   * @param pointerIndex - Pointer field index.
   * @returns The announced element count, `0` for a null or empty list.
   */
  getStructListLength(pointerIndex) {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? 0 : this.reader.readStructListLength(ptr);
  }
  getStruct(pointerIndex, dataWords, pointerWords) {
    const ptr = this.pointerWord(pointerIndex);
    if (ptr === null) {
      return new _StructReader(this.reader, 0, 0, 0);
    }
    return this.reader.structAt(ptr, dataWords, pointerWords);
  }
};

// dist/errors.js
var KNOWN_ERROR_CODES = new Set(PLUGIN_ERROR_CODES);
var MAX_DETAILS_BYTES = MAX_SCALAR_BYTES;
var LIMIT_NAME = /^[A-Za-z0-9_]+$/;
var UTF8 = new TextEncoder();
function isCount(n) {
  return typeof n === "number" && Number.isSafeInteger(n) && n >= 0;
}
var LIMIT_MESSAGE_PREFIX = "limit exceeded: ";
function limitExceededMessage(limit) {
  return `${LIMIT_MESSAGE_PREFIX}${limit.limit} is ${limit.cap}; used ${limit.used}`;
}
function parseLimitExceeded(message) {
  const start = message.indexOf(LIMIT_MESSAGE_PREFIX);
  if (start < 0)
    return void 0;
  const match = /^([A-Za-z0-9_]+) is (\d+); used (\d+)/.exec(message.slice(start + LIMIT_MESSAGE_PREFIX.length));
  if (!match)
    return void 0;
  const cap = Number(match[2]);
  const used = Number(match[3]);
  if (!Number.isSafeInteger(cap) || !Number.isSafeInteger(used))
    return void 0;
  return { limit: match[1] ?? "", cap, used };
}
function limitFromDetails(details) {
  const { limit, cap, used } = details;
  if (typeof limit !== "string" || !LIMIT_NAME.test(limit) || !isCount(cap) || !isCount(used)) {
    return void 0;
  }
  return { limit, cap, used };
}
function parseDetails(text) {
  if (text === "" || UTF8.encode(text).byteLength > MAX_DETAILS_BYTES)
    return void 0;
  try {
    const parsed = JSON.parse(text);
    if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
      return Object.freeze({ ...parsed });
    }
  } catch {
  }
  return void 0;
}
var PluginError = class _PluginError extends Error {
  /** Known `PluginErrorCode` wire string, or `unknown`. */
  code;
  /** Raw wire code, including codes this SDK does not know. */
  wireCode;
  /** Structured detail object from the wire (`details` JSON), when the error carries one. */
  details;
  /**
   * @param code - Wire error code.
   * @param message - Operator-facing error text.
   * @param details - Detail object, or its JSON text as carried on the wire.
   */
  constructor(code, message, details) {
    super(message);
    this.name = "PluginError";
    this.wireCode = code;
    this.code = KNOWN_ERROR_CODES.has(code) ? code : "unknown";
    this.details = typeof details === "string" ? parseDetails(details) : details === void 0 ? void 0 : Object.freeze({ ...details });
  }
  /**
   * Construct a {@link PluginError} from a Cap'n Proto / JSON wire code.
   *
   * Unknown codes become `unknown` on {@link PluginError.code} while
   * {@link PluginError.wireCode} keeps the raw value.
   *
   * @param code - Wire error code (known or unknown).
   * @param message - Operator-facing error text.
   * @param details - `details` JSON text from the wire (empty when none).
   * @returns Typed plugin error.
   */
  static fromWire(code, message, details = "") {
    return new _PluginError(code, message, details);
  }
  /**
   * Builds a `limit_exceeded` error whose message and `details` both carry
   * the exceeded cap.
   *
   * @param limit - Exceeded cap.
   * @returns Typed plugin error.
   */
  static limitExceeded(limit) {
    return new _PluginError("limit_exceeded", limitExceededMessage(limit), { ...limit });
  }
  /**
   * The exceeded cap when this is a `limit_exceeded` error.
   *
   * @returns The cap from `details` first, then from the canonical message;
   * `undefined` for every other code.
   */
  get limit() {
    if (this.wireCode !== "limit_exceeded")
      return void 0;
    return (this.details && limitFromDetails(this.details)) ?? parseLimitExceeded(this.message);
  }
  /**
   * `details` as the JSON text the wire carries.
   *
   * @returns JSON object text, or an empty string when the error has none.
   */
  get detailsText() {
    if (this.details === void 0)
      return "";
    try {
      return JSON.stringify(this.details) ?? "";
    } catch {
      return "";
    }
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
function fromOrdOr(table, ordinal, fallback) {
  return table[ordinal] ?? fallback;
}
function unknownUnion(struct, member) {
  return new Error(`unknown ${struct} union member: ${String(member)}`);
}
var EMPTY_BYTES = new Uint8Array(0);
var PluginErrorCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.code ?? "");
    s.setText(1, v.message ?? "");
    s.setText(2, v.details ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      code: s.getText(0),
      message: s.getText(1),
      details: s.getText(2)
    };
  }
};
var ObjectMetadataCodec = {
  dataWords: 2,
  pointerCount: 6,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
    s.setText(1, v.contentType ?? "");
    s.setText(2, v.etag ?? "");
    s.setData(3, v.sha256 ?? EMPTY_BYTES);
    s.setUint64(1, BigInt(v.modifiedAtMs ?? 0));
    s.setText(4, v.versionId ?? "");
    s.setText(5, v.signature ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0)),
      contentType: s.getText(1),
      etag: s.getText(2),
      sha256: s.getData(3),
      modifiedAtMs: Number(s.getUint64(1)),
      versionId: s.getText(4),
      signature: s.getText(5)
    };
  }
};
var ObjectInfoCodec = {
  dataWords: 2,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.size ?? 0));
    s.setText(1, v.etag ?? "");
    s.setUint64(1, BigInt(v.modifiedAtMs ?? 0));
    s.setText(2, v.versionId ?? "");
    s.setText(3, v.signature ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      size: Number(s.getUint64(0)),
      etag: s.getText(1),
      modifiedAtMs: Number(s.getUint64(1)),
      versionId: s.getText(2),
      signature: s.getText(3)
    };
  }
};
var ListOptionsCodec = {
  dataWords: 2,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.prefix ?? "");
    s.setText(1, v.cursor ?? "");
    s.setUint32(0, v.limit ?? 0);
    s.setBool(32, v.withSignatures ?? false);
    s.setUint64(1, BigInt(v.modifiedSinceMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      prefix: s.getText(0),
      cursor: s.getText(1),
      limit: s.getUint32(0),
      withSignatures: s.getBool(32),
      modifiedSinceMs: Number(s.getUint64(1))
    };
  }
};
var ListPageCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.objects ?? [];
      const items = s.initStructList(0, list.length, 2, 4);
      for (let i = 0; i < items.length; i++) {
        ObjectInfoCodec.write(items[i], list[i], caps);
      }
    }
    s.setText(1, v.nextCursor ?? "");
    s.setBool(0, v.signaturesIncluded ?? false);
    s.setBool(1, v.modifiedSinceApplied ?? false);
  },
  read(s, caps) {
    return {
      objects: s.getStructList(0, 2, 4).map((item) => ObjectInfoCodec.read(item, caps)),
      nextCursor: s.getText(1),
      signaturesIncluded: s.getBool(0),
      modifiedSinceApplied: s.getBool(1)
    };
  }
};
var PutResultCodec = {
  dataWords: 2,
  pointerCount: 5,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.key ?? "");
    s.setUint64(0, BigInt(v.bytesWritten ?? 0));
    s.setText(1, v.etag ?? "");
    s.setData(2, v.sha256 ?? EMPTY_BYTES);
    s.setUint64(1, BigInt(v.modifiedAtMs ?? 0));
    s.setText(3, v.versionId ?? "");
    s.setText(4, v.signature ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      key: s.getText(0),
      bytesWritten: Number(s.getUint64(0)),
      etag: s.getText(1),
      sha256: s.getData(2),
      modifiedAtMs: Number(s.getUint64(1)),
      versionId: s.getText(3),
      signature: s.getText(4)
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
var MoveOptionsCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.noOverwrite ?? false);
    s.setText(0, v.ifSourceMatch ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      noOverwrite: s.getBool(0),
      ifSourceMatch: s.getText(0)
    };
  }
};
var MoveResultCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.atomic ?? false);
    s.setBool(1, v.alreadyMoved ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      atomic: s.getBool(0),
      alreadyMoved: s.getBool(1)
    };
  }
};
var KeyPairCodec = {
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
var MoveManyTargetCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "pairs":
        s.setUint16(0, 0);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 0, 2);
          for (let i = 0; i < items.length; i++) {
            KeyPairCodec.write(items[i], list[i], caps);
          }
        }
        break;
      case "prefix":
        s.setUint16(0, 1);
        if (v.value != null) {
          KeyPairCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MoveManyTarget", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "pairs", value: s.getStructList(0, 0, 2).map((item) => KeyPairCodec.read(item, caps)) };
      case 1:
        return { kind: "prefix", value: KeyPairCodec.read(s.getStruct(0, 0, 2), caps) };
      default:
        throw unknownUnion("MoveManyTarget", disc);
    }
  }
};
var MoveManyRequestCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    if (v.target != null) {
      MoveManyTargetCodec.write(s.initStruct(0, 1, 1), v.target, caps);
    }
    s.setText(1, v.cursor ?? "");
    s.setUint32(0, v.limit ?? 0);
    if (v.options != null) {
      MoveOptionsCodec.write(s.initStruct(2, 1, 1), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      target: MoveManyTargetCodec.read(s.getStruct(0, 1, 1), caps),
      cursor: s.getText(1),
      limit: s.getUint32(0),
      options: MoveOptionsCodec.read(s.getStruct(2, 1, 1), caps)
    };
  }
};
var KeyOutcomeCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.key ?? "");
    if (v.error != null) {
      PluginErrorCodec.write(s.initStruct(1, 0, 3), v.error, caps);
    }
  },
  read(s, caps) {
    return {
      key: s.getText(0),
      error: PluginErrorCodec.read(s.getStruct(1, 0, 3), caps)
    };
  }
};
var MoveManyPageCodec = {
  dataWords: 2,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.results ?? [];
      const items = s.initStructList(0, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        KeyOutcomeCodec.write(items[i], list[i], caps);
      }
    }
    s.setText(1, v.nextCursor ?? "");
    s.setBool(0, v.atomic ?? false);
    s.setUint64(1, BigInt(v.objectsMoved ?? 0));
  },
  read(s, caps) {
    return {
      results: s.getStructList(0, 0, 2).map((item) => KeyOutcomeCodec.read(item, caps)),
      nextCursor: s.getText(1),
      atomic: s.getBool(0),
      objectsMoved: Number(s.getUint64(1))
    };
  }
};
var DeleteManyResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.results ?? [];
      const items = s.initStructList(0, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        KeyOutcomeCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      results: s.getStructList(0, 0, 2).map((item) => KeyOutcomeCodec.read(item, caps))
    };
  }
};
var HeadManyResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.objects ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        HeadOkCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      objects: s.getStructList(0, 1, 1).map((item) => HeadOkCodec.read(item, caps))
    };
  }
};
var TranslateKeysResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.keys ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      keys: s.getTextList(0)
    };
  }
};
var ChangesRequestCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.cursor ?? "");
    s.setUint32(0, v.limit ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      cursor: s.getText(0),
      limit: s.getUint32(0)
    };
  }
};
var ObjectChangeCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.key ?? "");
    s.setUint16(0, ord(OBJECT_CHANGE_KINDS, v.kind ?? OBJECT_CHANGE_KINDS[0], "ObjectChangeKind"));
    if (v.info != null) {
      ObjectInfoCodec.write(s.initStruct(1, 2, 4), v.info, caps);
    }
  },
  read(s, caps) {
    return {
      key: s.getText(0),
      kind: fromOrdOr(OBJECT_CHANGE_KINDS, s.getUint16(0), "changed"),
      info: ObjectInfoCodec.read(s.getStruct(1, 2, 4), caps)
    };
  }
};
var ChangesPageCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.changes ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        ObjectChangeCodec.write(items[i], list[i], caps);
      }
    }
    s.setText(1, v.cursor ?? "");
    s.setBool(0, v.more ?? false);
    s.setBool(1, v.complete ?? false);
    s.setBool(2, v.liveWatch ?? false);
  },
  read(s, caps) {
    return {
      changes: s.getStructList(0, 1, 2).map((item) => ObjectChangeCodec.read(item, caps)),
      cursor: s.getText(1),
      more: s.getBool(0),
      complete: s.getBool(1),
      liveWatch: s.getBool(2)
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
    if (v.input != null) {
      s.setCap(1, caps.exportCap(v.input));
    }
    if (v.output != null) {
      s.setCap(2, caps.exportCap(v.output));
    }
    if (v.progress != null) {
      s.setCap(3, caps.exportCap(v.progress));
    }
    if (v.cancel != null) {
      s.setCap(4, caps.exportCap(v.cancel));
    }
  },
  read(s, caps) {
    const out = {
      invocation: JobInvocationCodec.read(s.getStruct(0, 3, 8), caps)
    };
    const inputValue = caps.importCap(s.getCapIndex(1));
    if (!(inputValue == null)) {
      out.input = inputValue;
    }
    const outputValue = caps.importCap(s.getCapIndex(2));
    if (!(outputValue == null)) {
      out.output = outputValue;
    }
    const progressValue = caps.importCap(s.getCapIndex(3));
    if (!(progressValue == null)) {
      out.progress = progressValue;
    }
    const cancelValue = caps.importCap(s.getCapIndex(4));
    if (!(cancelValue == null)) {
      out.cancel = cancelValue;
    }
    return out;
  }
};
var HeadOkCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.meta != null) {
      ObjectMetadataCodec.write(s.initStruct(0, 2, 6), v.meta, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      meta: ObjectMetadataCodec.read(s.getStruct(0, 2, 6), caps)
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          ListPageCodec.write(s.initStruct(0, 1, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: ListPageCodec.read(s.getStruct(0, 1, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PutResultCodec.write(s.initStruct(0, 2, 5), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: PutResultCodec.read(s.getStruct(0, 2, 5), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("CopyReply", disc);
    }
  }
};
var MoveReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MoveResultCodec.write(s.initStruct(0, 1, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MoveReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MoveResultCodec.read(s.getStruct(0, 1, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MoveReply", disc);
    }
  }
};
var MoveManyReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MoveManyPageCodec.write(s.initStruct(0, 2, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MoveManyReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MoveManyPageCodec.read(s.getStruct(0, 2, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MoveManyReply", disc);
    }
  }
};
var DeleteManyReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          DeleteManyResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("DeleteManyReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: DeleteManyResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("DeleteManyReply", disc);
    }
  }
};
var HeadManyReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          HeadManyResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("HeadManyReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: HeadManyResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("HeadManyReply", disc);
    }
  }
};
var TranslateKeysReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          TranslateKeysResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("TranslateKeysReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: TranslateKeysResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("TranslateKeysReply", disc);
    }
  }
};
var ChangesReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          ChangesPageCodec.write(s.initStruct(0, 1, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ChangesReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: ChangesPageCodec.read(s.getStruct(0, 1, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("ChangesReply", disc);
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
        if (v.value != null) {
          s.setCap(0, caps.exportCap(v.value));
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
  pointerCount: 5,
  write(s, v, caps) {
    s.setText(0, v.pluginDataDir ?? "");
    s.setText(1, v.accountId ?? "");
    s.setUint32(0, v.pageSize ?? 0);
    s.setBool(32, v.importEpisodes ?? false);
    s.setBool(33, v.importPlusTitles ?? false);
    if (v.credential != null) {
      AccountCredentialCodec.write(s.initStruct(2, 0, 2), v.credential, caps);
    }
    s.setText(3, v.cursor ?? "");
    s.setText(4, v.syncToken ?? "");
  },
  read(s, caps) {
    return {
      pluginDataDir: s.getText(0),
      accountId: s.getText(1),
      pageSize: s.getUint32(0),
      importEpisodes: s.getBool(32),
      importPlusTitles: s.getBool(33),
      credential: AccountCredentialCodec.read(s.getStruct(2, 0, 2), caps),
      cursor: s.getText(3),
      syncToken: s.getText(4)
    };
  }
};
var IdentifierCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setUint16(0, ord(IDENTIFIER_SCHEMES, v.scheme ?? IDENTIFIER_SCHEMES[0], "IdentifierScheme"));
    s.setText(0, v.value ?? "");
    if (v.marketplace !== void 0) {
      s.setText(1, v.marketplace ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      scheme: fromOrd(IDENTIFIER_SCHEMES, s.getUint16(0), "IdentifierScheme"),
      value: s.getText(0)
    };
    const marketplaceValue = s.getText(1);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    return out;
  }
};
var CreditCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint16(0, ord(CREDIT_ROLES, v.role ?? CREDIT_ROLES[0], "CreditRole"));
    s.setUint16(1, ord(CREDIT_LEVELS, v.level ?? CREDIT_LEVELS[0], "CreditLevel"));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      role: fromOrd(CREDIT_ROLES, s.getUint16(0), "CreditRole"),
      level: fromOrd(CREDIT_LEVELS, s.getUint16(1), "CreditLevel")
    };
  }
};
var SeriesEntryCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    if (v.position !== void 0) {
      s.setText(1, v.position ?? "");
    }
    if (v.storeSeriesId !== void 0) {
      s.setText(2, v.storeSeriesId ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      name: s.getText(0)
    };
    const positionValue = s.getText(1);
    if (!(positionValue === "")) {
      out.position = positionValue;
    }
    const storeSeriesIdValue = s.getText(2);
    if (!(storeSeriesIdValue === "")) {
      out.storeSeriesId = storeSeriesIdValue;
    }
    return out;
  }
};
var ScanBookCodec = {
  dataWords: 3,
  pointerCount: 14,
  write(s, v, caps) {
    s.setText(0, v.accountId ?? "");
    s.setText(1, v.productId ?? "");
    s.setText(2, v.title ?? "");
    if (v.marketplace !== void 0) {
      s.setText(3, v.marketplace ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(4, v.publisher ?? "");
    }
    if (v.lengthMinutes !== void 0) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.subtitle !== void 0) {
      s.setText(5, v.subtitle ?? "");
    }
    s.setUint16(4, ord(WORK_KINDS, v.workKind ?? WORK_KINDS[0], "WorkKind"));
    s.setUint16(5, ord(EXPRESSION_KINDS, v.expressionKind ?? EXPRESSION_KINDS[0], "ExpressionKind"));
    s.setUint16(6, ord(MEDIA_KINDS, v.mediaKind ?? MEDIA_KINDS[0], "MediaKind"));
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(6, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.credits ?? [];
      const items = s.initStructList(7, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        CreditCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.seriesEntries ?? [];
      const items = s.initStructList(8, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        SeriesEntryCodec.write(items[i], list[i], caps);
      }
    }
    if (v.parentProductId !== void 0) {
      s.setText(9, v.parentProductId ?? "");
    }
    if (v.language !== void 0) {
      s.setText(10, v.language ?? "");
    }
    s.setUint16(7, ord(ABRIDGEMENTS, v.abridgement ?? ABRIDGEMENTS[0], "Abridgement"));
    if (v.publishedAt !== void 0) {
      s.setText(11, v.publishedAt ?? "");
    }
    if (v.purchasedAt !== void 0) {
      s.setText(12, v.purchasedAt ?? "");
    }
    if (v.categories !== void 0) {
      s.setText(13, v.categories ?? "");
    }
    s.setUint16(8, ord(EXPLICITNESSS, v.explicitness ?? EXPLICITNESSS[0], "Explicitness"));
  },
  read(s, caps) {
    const out = {
      accountId: s.getText(0),
      productId: s.getText(1),
      title: s.getText(2),
      workKind: fromOrd(WORK_KINDS, s.getUint16(4), "WorkKind"),
      expressionKind: fromOrd(EXPRESSION_KINDS, s.getUint16(5), "ExpressionKind"),
      mediaKind: fromOrd(MEDIA_KINDS, s.getUint16(6), "MediaKind"),
      identifiers: s.getStructList(6, 1, 2).map((item) => IdentifierCodec.read(item, caps)),
      credits: s.getStructList(7, 1, 1).map((item) => CreditCodec.read(item, caps)),
      seriesEntries: s.getStructList(8, 0, 3).map((item) => SeriesEntryCodec.read(item, caps)),
      abridgement: fromOrd(ABRIDGEMENTS, s.getUint16(7), "Abridgement"),
      explicitness: fromOrd(EXPLICITNESSS, s.getUint16(8), "Explicitness")
    };
    const marketplaceValue = s.getText(3);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    const publisherValue = s.getText(4);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const subtitleValue = s.getText(5);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const parentProductIdValue = s.getText(9);
    if (!(parentProductIdValue === "")) {
      out.parentProductId = parentProductIdValue;
    }
    const languageValue = s.getText(10);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const publishedAtValue = s.getText(11);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const purchasedAtValue = s.getText(12);
    if (!(purchasedAtValue === "")) {
      out.purchasedAt = purchasedAtValue;
    }
    const categoriesValue = s.getText(13);
    if (!(categoriesValue === "")) {
      out.categories = categoriesValue;
    }
    return out;
  }
};
var ScanPageCodec = {
  dataWords: 1,
  pointerCount: 5,
  write(s, v, caps) {
    {
      const list = v.books ?? [];
      const items = s.initStructList(0, list.length, 3, 14);
      for (let i = 0; i < items.length; i++) {
        ScanBookCodec.write(items[i], list[i], caps);
      }
    }
    s.setTextList(1, v.removed ?? []);
    s.setText(2, v.nextCursor ?? "");
    s.setText(3, v.syncToken ?? "");
    s.setBool(0, v.complete ?? false);
    s.setUint32(1, v.storePages ?? 0);
    if (v.refreshedCredentials !== void 0) {
      s.setData(4, v.refreshedCredentials ?? EMPTY_BYTES);
    }
  },
  read(s, caps) {
    const out = {
      books: s.getStructList(0, 3, 14).map((item) => ScanBookCodec.read(item, caps)),
      removed: s.getTextList(1),
      nextCursor: s.getText(2),
      syncToken: s.getText(3),
      complete: s.getBool(0),
      storePages: s.getUint32(1)
    };
    const refreshedCredentialsValue = s.getData(4);
    if (!(refreshedCredentialsValue.length === 0)) {
      out.refreshedCredentials = refreshedCredentialsValue;
    }
    return out;
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
          ScanPageCodec.write(s.initStruct(0, 1, 5), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: ScanPageCodec.read(s.getStruct(0, 1, 5), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
var FetchedFileCodec = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.path ?? "");
    s.setUint16(0, ord(FETCHED_FILE_ROLES, v.role ?? FETCHED_FILE_ROLES[0], "FetchedFileRole"));
    if (v.contentType !== void 0) {
      s.setText(1, v.contentType ?? "");
    }
    if (v.title !== void 0) {
      s.setText(2, v.title ?? "");
    }
    if (v.durationMs !== void 0) {
      s.setUint64(1, BigInt(v.durationMs ?? 0));
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      path: s.getText(0),
      role: fromOrd(FETCHED_FILE_ROLES, s.getUint16(0), "FetchedFileRole")
    };
    const contentTypeValue = s.getText(1);
    if (!(contentTypeValue === "")) {
      out.contentType = contentTypeValue;
    }
    const titleValue = s.getText(2);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const durationMsValue = Number(s.getUint64(1));
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
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    s.setUint16(0, ord(MEDIA_KINDS, v.mediaKind ?? MEDIA_KINDS[0], "MediaKind"));
    {
      const list = v.files ?? [];
      const items = s.initStructList(0, list.length, 2, 3);
      for (let i = 0; i < items.length; i++) {
        FetchedFileCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.chapters ?? [];
      const items = s.initStructList(1, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        ChapterMarkerCodec.write(items[i], list[i], caps);
      }
    }
    if (v.pdfUrl !== void 0) {
      s.setText(2, v.pdfUrl ?? "");
    }
    if (v.refreshedCredentials !== void 0) {
      s.setData(3, v.refreshedCredentials ?? EMPTY_BYTES);
    }
    s.setUint16(1, ord(CHAPTER_KINDS, v.chapterKind ?? CHAPTER_KINDS[0], "ChapterKind"));
  },
  read(s, caps) {
    const out = {
      mediaKind: fromOrd(MEDIA_KINDS, s.getUint16(0), "MediaKind"),
      files: s.getStructList(0, 2, 3).map((item) => FetchedFileCodec.read(item, caps)),
      chapters: s.getStructList(1, 1, 1).map((item) => ChapterMarkerCodec.read(item, caps)),
      chapterKind: fromOrd(CHAPTER_KINDS, s.getUint16(1), "ChapterKind")
    };
    const pdfUrlValue = s.getText(2);
    if (!(pdfUrlValue === "")) {
      out.pdfUrl = pdfUrlValue;
    }
    const refreshedCredentialsValue = s.getData(3);
    if (!(refreshedCredentialsValue.length === 0)) {
      out.refreshedCredentials = refreshedCredentialsValue;
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
          PlainFetchCodec.write(s.initStruct(0, 1, 4), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: PlainFetchCodec.read(s.getStruct(0, 1, 4), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
    s.setUint16(1, ord(PRICE_LOOKUPS, v.lookup ?? PRICE_LOOKUPS[0], "PriceLookup"));
  },
  read(s, caps) {
    void caps;
    const out = {
      region: s.getText(5),
      withPrice: s.getBool(0),
      lookup: fromOrd(PRICE_LOOKUPS, s.getUint16(1), "PriceLookup")
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
var RevokeAccountParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.accountId ?? "");
    if (v.credentials !== void 0) {
      s.setData(1, v.credentials ?? EMPTY_BYTES);
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      accountId: s.getText(0)
    };
    const credentialsValue = s.getData(1);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  }
};
var CatalogHitCodec = {
  dataWords: 6,
  pointerCount: 17,
  write(s, v, caps) {
    s.setText(0, v.productId ?? "");
    s.setText(1, v.title ?? "");
    if (v.url !== void 0) {
      s.setText(2, v.url ?? "");
    }
    if (v.coverUrl !== void 0) {
      s.setText(3, v.coverUrl ?? "");
    }
    s.setText(4, v.origin ?? "");
    if (v.subtitle !== void 0) {
      s.setText(5, v.subtitle ?? "");
    }
    if (v.description !== void 0) {
      s.setText(6, v.description ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(7, v.publisher ?? "");
    }
    if (v.lengthMinutes !== void 0) {
      s.setInt64(0, v.lengthMinutes ?? 0);
    }
    if (v.publishedAt !== void 0) {
      s.setText(8, v.publishedAt ?? "");
    }
    if (v.categories !== void 0) {
      s.setText(9, v.categories ?? "");
    }
    if (v.language !== void 0) {
      s.setText(10, v.language ?? "");
    }
    if (v.priceCents !== void 0) {
      s.setInt64(1, v.priceCents ?? 0);
    }
    if (v.currency !== void 0) {
      s.setText(11, v.currency ?? "");
    }
    if (v.priceLabel !== void 0) {
      s.setText(12, v.priceLabel ?? "");
    }
    if (v.ratingOverall !== void 0) {
      s.setFloat64(2, v.ratingOverall ?? 0);
    }
    if (v.ratingCount !== void 0) {
      s.setInt64(3, v.ratingCount ?? 0);
    }
    s.setUint16(16, ord(ABRIDGEMENTS, v.abridgement ?? ABRIDGEMENTS[0], "Abridgement"));
    s.setUint16(17, ord(WORK_KINDS, v.workKind ?? WORK_KINDS[0], "WorkKind"));
    s.setUint16(18, ord(EXPRESSION_KINDS, v.expressionKind ?? EXPRESSION_KINDS[0], "ExpressionKind"));
    s.setUint16(19, ord(MEDIA_KINDS, v.mediaKind ?? MEDIA_KINDS[0], "MediaKind"));
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(13, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.credits ?? [];
      const items = s.initStructList(14, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        CreditCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.seriesEntries ?? [];
      const items = s.initStructList(15, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        SeriesEntryCodec.write(items[i], list[i], caps);
      }
    }
    if (v.parentProductId !== void 0) {
      s.setText(16, v.parentProductId ?? "");
    }
    if (v.rankHint !== void 0) {
      s.setUint32(10, v.rankHint ?? 0);
    }
    s.setUint16(22, ord(EXPLICITNESSS, v.explicitness ?? EXPLICITNESSS[0], "Explicitness"));
  },
  read(s, caps) {
    const out = {
      productId: s.getText(0),
      title: s.getText(1),
      origin: s.getText(4),
      abridgement: fromOrd(ABRIDGEMENTS, s.getUint16(16), "Abridgement"),
      workKind: fromOrd(WORK_KINDS, s.getUint16(17), "WorkKind"),
      expressionKind: fromOrd(EXPRESSION_KINDS, s.getUint16(18), "ExpressionKind"),
      mediaKind: fromOrd(MEDIA_KINDS, s.getUint16(19), "MediaKind"),
      identifiers: s.getStructList(13, 1, 2).map((item) => IdentifierCodec.read(item, caps)),
      credits: s.getStructList(14, 1, 1).map((item) => CreditCodec.read(item, caps)),
      seriesEntries: s.getStructList(15, 0, 3).map((item) => SeriesEntryCodec.read(item, caps)),
      explicitness: fromOrd(EXPLICITNESSS, s.getUint16(22), "Explicitness")
    };
    const urlValue = s.getText(2);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const coverUrlValue = s.getText(3);
    if (!(coverUrlValue === "")) {
      out.coverUrl = coverUrlValue;
    }
    const subtitleValue = s.getText(5);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const descriptionValue = s.getText(6);
    if (!(descriptionValue === "")) {
      out.description = descriptionValue;
    }
    const publisherValue = s.getText(7);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getInt64(0);
    if (!(lengthMinutesValue === 0n)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const publishedAtValue = s.getText(8);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const categoriesValue = s.getText(9);
    if (!(categoriesValue === "")) {
      out.categories = categoriesValue;
    }
    const languageValue = s.getText(10);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const priceCentsValue = s.getInt64(1);
    if (!(priceCentsValue === 0n)) {
      out.priceCents = priceCentsValue;
    }
    const currencyValue = s.getText(11);
    if (!(currencyValue === "")) {
      out.currency = currencyValue;
    }
    const priceLabelValue = s.getText(12);
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
    const parentProductIdValue = s.getText(16);
    if (!(parentProductIdValue === "")) {
      out.parentProductId = parentProductIdValue;
    }
    const rankHintValue = s.getUint32(10);
    if (!(rankHintValue === 0)) {
      out.rankHint = rankHintValue;
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
      const items = s.initStructList(0, list.length, 6, 17);
      for (let i = 0; i < items.length; i++) {
        CatalogHitCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      hits: s.getStructList(0, 6, 17).map((item) => CatalogHitCodec.read(item, caps))
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
      CatalogHitCodec.write(s.initStruct(0, 6, 17), v.hit, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hit: CatalogHitCodec.read(s.getStruct(0, 6, 17), caps)
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
  pointerCount: 2,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.hint != null) {
      PurchaseHintCodec.write(s.initStruct(0, 3, 7), v.hint, caps);
    }
    {
      const list = v.records ?? [];
      const items = s.initStructList(1, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        PricedRecordCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      hint: PurchaseHintCodec.read(s.getStruct(0, 3, 7), caps),
      records: s.getStructList(1, 0, 2).map((item) => PricedRecordCodec.read(item, caps))
    };
  }
};
var PricedRecordCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.record != null) {
      CatalogHitCodec.write(s.initStruct(0, 6, 17), v.record, caps);
    }
    if (v.hint != null) {
      PurchaseHintCodec.write(s.initStruct(1, 3, 7), v.hint, caps);
    }
  },
  read(s, caps) {
    return {
      record: CatalogHitCodec.read(s.getStruct(0, 6, 17), caps),
      hint: PurchaseHintCodec.read(s.getStruct(1, 3, 7), caps)
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
          PurchaseHintResultCodec.write(s.initStruct(0, 1, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: PurchaseHintResultCodec.read(s.getStruct(0, 1, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("PurchaseHintReply", disc);
    }
  }
};
var PurchaseHintsParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.queries ?? [];
      const items = s.initStructList(0, list.length, 1, 6);
      for (let i = 0; i < items.length; i++) {
        PurchaseHintParamsCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      queries: s.getStructList(0, 1, 6).map((item) => PurchaseHintParamsCodec.read(item, caps))
    };
  }
};
var PurchaseHintsResultCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.results ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        PurchaseHintResultCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      results: s.getStructList(0, 1, 2).map((item) => PurchaseHintResultCodec.read(item, caps))
    };
  }
};
var PurchaseHintsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          PurchaseHintsResultCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("PurchaseHintsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: PurchaseHintsResultCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("PurchaseHintsReply", disc);
    }
  }
};
var QualityOfferCodec = {
  dataWords: 2,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setUint16(0, ord(OFFERED_CODECS, v.codec ?? OFFERED_CODECS[0], "OfferedCodec"));
    s.setUint16(1, ord(OFFERED_AAC_PROFILES, v.aacProfile ?? OFFERED_AAC_PROFILES[0], "OfferedAacProfile"));
    if (v.bitrateKbps !== void 0) {
      s.setUint32(1, v.bitrateKbps ?? 0);
    }
    if (v.sampleRateHz !== void 0) {
      s.setUint32(2, v.sampleRateHz ?? 0);
    }
    if (v.channels !== void 0) {
      s.setUint32(3, v.channels ?? 0);
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      codec: fromOrd(OFFERED_CODECS, s.getUint16(0), "OfferedCodec"),
      aacProfile: fromOrd(OFFERED_AAC_PROFILES, s.getUint16(1), "OfferedAacProfile")
    };
    const bitrateKbpsValue = s.getUint32(1);
    if (!(bitrateKbpsValue === 0)) {
      out.bitrateKbps = bitrateKbpsValue;
    }
    const sampleRateHzValue = s.getUint32(2);
    if (!(sampleRateHzValue === 0)) {
      out.sampleRateHz = sampleRateHzValue;
    }
    const channelsValue = s.getUint32(3);
    if (!(channelsValue === 0)) {
      out.channels = channelsValue;
    }
    return out;
  }
};
var QualityOffersParamsCodec = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    s.setText(0, v.accountId ?? "");
    if (v.credentials !== void 0) {
      s.setData(1, v.credentials ?? EMPTY_BYTES);
    }
    if (v.sourceConfig != null) {
      ExtensibleConfigCodec.write(s.initStruct(2, 1, 2), v.sourceConfig, caps);
    }
    s.setTextList(3, v.titleIds ?? []);
    if (v.fetch != null) {
      FetchOptionsCodec.write(s.initStruct(4, 1, 4), v.fetch, caps);
    }
  },
  read(s, caps) {
    const out = {
      accountId: s.getText(0),
      sourceConfig: ExtensibleConfigCodec.read(s.getStruct(2, 1, 2), caps),
      titleIds: s.getTextList(3),
      fetch: FetchOptionsCodec.read(s.getStruct(4, 1, 4), caps)
    };
    const credentialsValue = s.getData(1);
    if (!(credentialsValue.length === 0)) {
      out.credentials = credentialsValue;
    }
    return out;
  }
};
var TitleQualityOfferCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    if (v.offer != null) {
      QualityOfferCodec.write(s.initStruct(0, 2, 0), v.offer, caps);
    }
  },
  read(s, caps) {
    return {
      found: s.getBool(0),
      offer: QualityOfferCodec.read(s.getStruct(0, 2, 0), caps)
    };
  }
};
var QualityOffersResultCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.offers ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        TitleQualityOfferCodec.write(items[i], list[i], caps);
      }
    }
    if (v.refreshedCredentials !== void 0) {
      s.setData(1, v.refreshedCredentials ?? EMPTY_BYTES);
    }
  },
  read(s, caps) {
    const out = {
      offers: s.getStructList(0, 1, 1).map((item) => TitleQualityOfferCodec.read(item, caps))
    };
    const refreshedCredentialsValue = s.getData(1);
    if (!(refreshedCredentialsValue.length === 0)) {
      out.refreshedCredentials = refreshedCredentialsValue;
    }
    return out;
  }
};
var QualityOffersReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          QualityOffersResultCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("QualityOffersReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: QualityOffersResultCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("QualityOffersReply", disc);
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("SyncListeningReply", disc);
    }
  }
};
var QueueMessageCodec = {
  dataWords: 2,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.id ?? "");
    s.setUint64(0, BigInt(v.timestampUnixMs ?? 0));
    s.setData(1, v.body ?? EMPTY_BYTES);
    s.setText(2, v.contentType ?? "");
    s.setUint32(2, v.attempts ?? 0);
    s.setText(3, v.source ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      id: s.getText(0),
      timestampUnixMs: Number(s.getUint64(0)),
      body: s.getData(1),
      contentType: s.getText(2),
      attempts: s.getUint32(2),
      source: s.getText(3)
    };
  }
};
var QueueBatchCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.queue ?? "");
    {
      const list = v.messages ?? [];
      const items = s.initStructList(1, list.length, 2, 4);
      for (let i = 0; i < items.length; i++) {
        QueueMessageCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      queue: s.getText(0),
      messages: s.getStructList(1, 2, 4).map((item) => QueueMessageCodec.read(item, caps))
    };
  }
};
var QueueRetryCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.delaySeconds ?? 0);
    s.setText(0, v.reason ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      delaySeconds: s.getUint32(0),
      reason: s.getText(0)
    };
  }
};
var QueueResultCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ack":
        s.setUint16(0, 0);
        break;
      case "retry":
        s.setUint16(0, 1);
        if (v.value != null) {
          QueueRetryCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("QueueResult", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ack" };
      case 1:
        return { kind: "retry", value: QueueRetryCodec.read(s.getStruct(0, 1, 1), caps) };
      default:
        throw unknownUnion("QueueResult", disc);
    }
  }
};
var QueueBatchOkCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.results ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        QueueResultCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      results: s.getStructList(0, 1, 1).map((item) => QueueResultCodec.read(item, caps))
    };
  }
};
var QueueBatchReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          QueueBatchOkCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("QueueBatchReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: QueueBatchOkCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("QueueBatchReply", disc);
    }
  }
};
var ScheduledControllerCodec = {
  dataWords: 3,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.cron ?? "");
    s.setUint64(0, BigInt(v.scheduledTime ?? 0));
    s.setText(1, v.fireId ?? "");
    s.setUint32(2, v.attempt ?? 0);
    s.setUint64(2, BigInt(v.deadlineUnixMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      cron: s.getText(0),
      scheduledTime: Number(s.getUint64(0)),
      fireId: s.getText(1),
      attempt: s.getUint32(2),
      deadlineUnixMs: Number(s.getUint64(2))
    };
  }
};
var MetadataLookupParamsCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    if (v.marketplace !== void 0) {
      s.setText(1, v.marketplace ?? "");
    }
    if (v.limit !== void 0) {
      s.setUint32(0, v.limit ?? 0);
    }
  },
  read(s, caps) {
    const out = {
      identifiers: s.getStructList(0, 1, 2).map((item) => IdentifierCodec.read(item, caps))
    };
    const marketplaceValue = s.getText(1);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    const limitValue = s.getUint32(0);
    if (!(limitValue === 0)) {
      out.limit = limitValue;
    }
    return out;
  }
};
var MetadataSearchParamsCodec = {
  dataWords: 1,
  pointerCount: 4,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.title ?? "");
    if (v.author !== void 0) {
      s.setText(1, v.author ?? "");
    }
    if (v.language !== void 0) {
      s.setText(2, v.language ?? "");
    }
    if (v.marketplace !== void 0) {
      s.setText(3, v.marketplace ?? "");
    }
    if (v.limit !== void 0) {
      s.setUint32(0, v.limit ?? 0);
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      title: s.getText(0)
    };
    const authorValue = s.getText(1);
    if (!(authorValue === "")) {
      out.author = authorValue;
    }
    const languageValue = s.getText(2);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const marketplaceValue = s.getText(3);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    const limitValue = s.getUint32(0);
    if (!(limitValue === 0)) {
      out.limit = limitValue;
    }
    return out;
  }
};
var MetadataChaptersParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(0, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    if (v.marketplace !== void 0) {
      s.setText(1, v.marketplace ?? "");
    }
  },
  read(s, caps) {
    const out = {
      identifiers: s.getStructList(0, 1, 2).map((item) => IdentifierCodec.read(item, caps))
    };
    const marketplaceValue = s.getText(1);
    if (!(marketplaceValue === "")) {
      out.marketplace = marketplaceValue;
    }
    return out;
  }
};
var MetadataRecordCodec = {
  dataWords: 3,
  pointerCount: 12,
  write(s, v, caps) {
    s.setText(0, v.title ?? "");
    if (v.subtitle !== void 0) {
      s.setText(1, v.subtitle ?? "");
    }
    if (v.description !== void 0) {
      s.setText(2, v.description ?? "");
    }
    if (v.coverUrl !== void 0) {
      s.setText(3, v.coverUrl ?? "");
    }
    if (v.url !== void 0) {
      s.setText(4, v.url ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(5, v.publisher ?? "");
    }
    if (v.lengthMinutes !== void 0) {
      s.setUint32(0, v.lengthMinutes ?? 0);
    }
    if (v.publishedAt !== void 0) {
      s.setText(6, v.publishedAt ?? "");
    }
    if (v.categories !== void 0) {
      s.setText(7, v.categories ?? "");
    }
    if (v.language !== void 0) {
      s.setText(8, v.language ?? "");
    }
    s.setUint16(2, ord(ABRIDGEMENTS, v.abridgement ?? ABRIDGEMENTS[0], "Abridgement"));
    s.setUint16(3, ord(WORK_KINDS, v.workKind ?? WORK_KINDS[0], "WorkKind"));
    s.setUint16(4, ord(EXPRESSION_KINDS, v.expressionKind ?? EXPRESSION_KINDS[0], "ExpressionKind"));
    s.setUint16(5, ord(MEDIA_KINDS, v.mediaKind ?? MEDIA_KINDS[0], "MediaKind"));
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(9, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.credits ?? [];
      const items = s.initStructList(10, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        CreditCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.seriesEntries ?? [];
      const items = s.initStructList(11, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        SeriesEntryCodec.write(items[i], list[i], caps);
      }
    }
    if (v.ratingOverall !== void 0) {
      s.setFloat64(2, v.ratingOverall ?? 0);
    }
    if (v.ratingCount !== void 0) {
      s.setUint32(3, v.ratingCount ?? 0);
    }
  },
  read(s, caps) {
    const out = {
      title: s.getText(0),
      abridgement: fromOrd(ABRIDGEMENTS, s.getUint16(2), "Abridgement"),
      workKind: fromOrd(WORK_KINDS, s.getUint16(3), "WorkKind"),
      expressionKind: fromOrd(EXPRESSION_KINDS, s.getUint16(4), "ExpressionKind"),
      mediaKind: fromOrd(MEDIA_KINDS, s.getUint16(5), "MediaKind"),
      identifiers: s.getStructList(9, 1, 2).map((item) => IdentifierCodec.read(item, caps)),
      credits: s.getStructList(10, 1, 1).map((item) => CreditCodec.read(item, caps)),
      seriesEntries: s.getStructList(11, 0, 3).map((item) => SeriesEntryCodec.read(item, caps))
    };
    const subtitleValue = s.getText(1);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const descriptionValue = s.getText(2);
    if (!(descriptionValue === "")) {
      out.description = descriptionValue;
    }
    const coverUrlValue = s.getText(3);
    if (!(coverUrlValue === "")) {
      out.coverUrl = coverUrlValue;
    }
    const urlValue = s.getText(4);
    if (!(urlValue === "")) {
      out.url = urlValue;
    }
    const publisherValue = s.getText(5);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const lengthMinutesValue = s.getUint32(0);
    if (!(lengthMinutesValue === 0)) {
      out.lengthMinutes = lengthMinutesValue;
    }
    const publishedAtValue = s.getText(6);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const categoriesValue = s.getText(7);
    if (!(categoriesValue === "")) {
      out.categories = categoriesValue;
    }
    const languageValue = s.getText(8);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const ratingOverallValue = s.getFloat64(2);
    if (!(ratingOverallValue === 0)) {
      out.ratingOverall = ratingOverallValue;
    }
    const ratingCountValue = s.getUint32(3);
    if (!(ratingCountValue === 0)) {
      out.ratingCount = ratingCountValue;
    }
    return out;
  }
};
var MetadataRecordsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.records ?? [];
      const items = s.initStructList(0, list.length, 3, 12);
      for (let i = 0; i < items.length; i++) {
        MetadataRecordCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      records: s.getStructList(0, 3, 12).map((item) => MetadataRecordCodec.read(item, caps))
    };
  }
};
var MetadataRecordsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MetadataRecordsCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MetadataRecordsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MetadataRecordsCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MetadataRecordsReply", disc);
    }
  }
};
var MetadataChapterCodec = {
  dataWords: 3,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.title ?? "");
    s.setUint64(0, BigInt(v.startMs ?? 0));
    if (v.lengthMs !== void 0) {
      s.setUint64(1, BigInt(v.lengthMs ?? 0));
    }
    s.setUint32(4, v.depth ?? 0);
  },
  read(s, caps) {
    void caps;
    const out = {
      title: s.getText(0),
      startMs: Number(s.getUint64(0)),
      depth: s.getUint32(4)
    };
    const lengthMsValue = Number(s.getUint64(1));
    if (!(lengthMsValue === 0)) {
      out.lengthMs = lengthMsValue;
    }
    return out;
  }
};
var MetadataChaptersCodec = {
  dataWords: 4,
  pointerCount: 1,
  write(s, v, caps) {
    s.setBool(0, v.found ?? false);
    {
      const list = v.chapters ?? [];
      const items = s.initStructList(0, list.length, 3, 1);
      for (let i = 0; i < items.length; i++) {
        MetadataChapterCodec.write(items[i], list[i], caps);
      }
    }
    if (v.brandIntroMs !== void 0) {
      s.setUint64(1, BigInt(v.brandIntroMs ?? 0));
    }
    if (v.brandOutroMs !== void 0) {
      s.setUint64(2, BigInt(v.brandOutroMs ?? 0));
    }
    if (v.runtimeMs !== void 0) {
      s.setUint64(3, BigInt(v.runtimeMs ?? 0));
    }
    s.setBool(1, v.accurate ?? false);
  },
  read(s, caps) {
    const out = {
      found: s.getBool(0),
      chapters: s.getStructList(0, 3, 1).map((item) => MetadataChapterCodec.read(item, caps)),
      accurate: s.getBool(1)
    };
    const brandIntroMsValue = Number(s.getUint64(1));
    if (!(brandIntroMsValue === 0)) {
      out.brandIntroMs = brandIntroMsValue;
    }
    const brandOutroMsValue = Number(s.getUint64(2));
    if (!(brandOutroMsValue === 0)) {
      out.brandOutroMs = brandOutroMsValue;
    }
    const runtimeMsValue = Number(s.getUint64(3));
    if (!(runtimeMsValue === 0)) {
      out.runtimeMs = runtimeMsValue;
    }
    return out;
  }
};
var MetadataChaptersReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MetadataChaptersCodec.write(s.initStruct(0, 4, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MetadataChaptersReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MetadataChaptersCodec.read(s.getStruct(0, 4, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MetadataChaptersReply", disc);
    }
  }
};
var CoverFetchParamsCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    s.setTextList(0, v.urls ?? []);
    s.setUint32(0, v.maxBytes ?? 0);
    {
      const list = v.conditions ?? [];
      const items = s.initStructList(1, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        CoverConditionCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      urls: s.getTextList(0),
      maxBytes: s.getUint32(0),
      conditions: s.getStructList(1, 0, 2).map((item) => CoverConditionCodec.read(item, caps))
    };
  }
};
var CoverConditionCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    if (v.ifNoneMatch !== void 0) {
      s.setText(0, v.ifNoneMatch ?? "");
    }
    if (v.ifModifiedSince !== void 0) {
      s.setText(1, v.ifModifiedSince ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {};
    const ifNoneMatchValue = s.getText(0);
    if (!(ifNoneMatchValue === "")) {
      out.ifNoneMatch = ifNoneMatchValue;
    }
    const ifModifiedSinceValue = s.getText(1);
    if (!(ifModifiedSinceValue === "")) {
      out.ifModifiedSince = ifModifiedSinceValue;
    }
    return out;
  }
};
var CoverImageCodec = {
  dataWords: 1,
  pointerCount: 9,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.url ?? "");
    s.setBool(0, v.stored ?? false);
    if (v.contentType !== void 0) {
      s.setText(1, v.contentType ?? "");
    }
    if (v.error !== void 0) {
      s.setText(2, v.error ?? "");
    }
    s.setBool(1, v.notModified ?? false);
    if (v.etag !== void 0) {
      s.setText(3, v.etag ?? "");
    }
    if (v.lastModified !== void 0) {
      s.setText(4, v.lastModified ?? "");
    }
    if (v.cacheControl !== void 0) {
      s.setText(5, v.cacheControl ?? "");
    }
    if (v.expires !== void 0) {
      s.setText(6, v.expires ?? "");
    }
    if (v.date !== void 0) {
      s.setText(7, v.date ?? "");
    }
    if (v.age !== void 0) {
      s.setText(8, v.age ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      url: s.getText(0),
      stored: s.getBool(0),
      notModified: s.getBool(1)
    };
    const contentTypeValue = s.getText(1);
    if (!(contentTypeValue === "")) {
      out.contentType = contentTypeValue;
    }
    const errorValue = s.getText(2);
    if (!(errorValue === "")) {
      out.error = errorValue;
    }
    const etagValue = s.getText(3);
    if (!(etagValue === "")) {
      out.etag = etagValue;
    }
    const lastModifiedValue = s.getText(4);
    if (!(lastModifiedValue === "")) {
      out.lastModified = lastModifiedValue;
    }
    const cacheControlValue = s.getText(5);
    if (!(cacheControlValue === "")) {
      out.cacheControl = cacheControlValue;
    }
    const expiresValue = s.getText(6);
    if (!(expiresValue === "")) {
      out.expires = expiresValue;
    }
    const dateValue = s.getText(7);
    if (!(dateValue === "")) {
      out.date = dateValue;
    }
    const ageValue = s.getText(8);
    if (!(ageValue === "")) {
      out.age = ageValue;
    }
    return out;
  }
};
var CoverImagesCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.images ?? [];
      const items = s.initStructList(0, list.length, 1, 9);
      for (let i = 0; i < items.length; i++) {
        CoverImageCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      images: s.getStructList(0, 1, 9).map((item) => CoverImageCodec.read(item, caps))
    };
  }
};
var CoverImagesReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          CoverImagesCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("CoverImagesReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: CoverImagesCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("CoverImagesReply", disc);
    }
  }
};
var MetadataFileRecordCodec = {
  dataWords: 2,
  pointerCount: 14,
  write(s, v, caps) {
    s.setUint16List(0, (v.present ?? []).map((v2) => ord(METADATA_FIELDS, v2, "MetadataField")));
    if (v.title !== void 0) {
      s.setText(1, v.title ?? "");
    }
    if (v.subtitle !== void 0) {
      s.setText(2, v.subtitle ?? "");
    }
    if (v.description !== void 0) {
      s.setText(3, v.description ?? "");
    }
    if (v.publisher !== void 0) {
      s.setText(4, v.publisher ?? "");
    }
    if (v.publishedAt !== void 0) {
      s.setText(5, v.publishedAt ?? "");
    }
    if (v.language !== void 0) {
      s.setText(6, v.language ?? "");
    }
    s.setTextList(7, v.genres ?? []);
    s.setTextList(8, v.tags ?? []);
    s.setUint16(0, ord(ABRIDGEMENTS, v.abridgement ?? ABRIDGEMENTS[0], "Abridgement"));
    s.setUint16(1, ord(EXPRESSION_KINDS, v.expressionKind ?? EXPRESSION_KINDS[0], "ExpressionKind"));
    {
      const list = v.identifiers ?? [];
      const items = s.initStructList(9, list.length, 1, 2);
      for (let i = 0; i < items.length; i++) {
        IdentifierCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.credits ?? [];
      const items = s.initStructList(10, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        CreditCodec.write(items[i], list[i], caps);
      }
    }
    s.setUint16List(11, (v.creditRoles ?? []).map((v2) => ord(CREDIT_ROLES, v2, "CreditRole")));
    {
      const list = v.seriesEntries ?? [];
      const items = s.initStructList(12, list.length, 0, 3);
      for (let i = 0; i < items.length; i++) {
        SeriesEntryCodec.write(items[i], list[i], caps);
      }
    }
    {
      const list = v.chapters ?? [];
      const items = s.initStructList(13, list.length, 3, 1);
      for (let i = 0; i < items.length; i++) {
        MetadataChapterCodec.write(items[i], list[i], caps);
      }
    }
    if (v.durationMs !== void 0) {
      s.setUint64(1, BigInt(v.durationMs ?? 0));
    }
    s.setUint16(2, ord(EXPLICITNESSS, v.explicitness ?? EXPLICITNESSS[0], "Explicitness"));
  },
  read(s, caps) {
    const out = {
      present: s.getUint16List(0).map((v) => fromOrd(METADATA_FIELDS, v, "MetadataField")),
      genres: s.getTextList(7),
      tags: s.getTextList(8),
      abridgement: fromOrd(ABRIDGEMENTS, s.getUint16(0), "Abridgement"),
      expressionKind: fromOrd(EXPRESSION_KINDS, s.getUint16(1), "ExpressionKind"),
      identifiers: s.getStructList(9, 1, 2).map((item) => IdentifierCodec.read(item, caps)),
      credits: s.getStructList(10, 1, 1).map((item) => CreditCodec.read(item, caps)),
      creditRoles: s.getUint16List(11).map((v) => fromOrd(CREDIT_ROLES, v, "CreditRole")),
      seriesEntries: s.getStructList(12, 0, 3).map((item) => SeriesEntryCodec.read(item, caps)),
      chapters: s.getStructList(13, 3, 1).map((item) => MetadataChapterCodec.read(item, caps)),
      explicitness: fromOrd(EXPLICITNESSS, s.getUint16(2), "Explicitness")
    };
    const titleValue = s.getText(1);
    if (!(titleValue === "")) {
      out.title = titleValue;
    }
    const subtitleValue = s.getText(2);
    if (!(subtitleValue === "")) {
      out.subtitle = subtitleValue;
    }
    const descriptionValue = s.getText(3);
    if (!(descriptionValue === "")) {
      out.description = descriptionValue;
    }
    const publisherValue = s.getText(4);
    if (!(publisherValue === "")) {
      out.publisher = publisherValue;
    }
    const publishedAtValue = s.getText(5);
    if (!(publishedAtValue === "")) {
      out.publishedAt = publishedAtValue;
    }
    const languageValue = s.getText(6);
    if (!(languageValue === "")) {
      out.language = languageValue;
    }
    const durationMsValue = Number(s.getUint64(1));
    if (!(durationMsValue === 0)) {
      out.durationMs = durationMsValue;
    }
    return out;
  }
};
var MetadataFileParseParamsCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.dialect ?? "");
    s.setText(1, v.fileName ?? "");
    s.setData(2, v.bytes ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      dialect: s.getText(0),
      fileName: s.getText(1),
      bytes: s.getData(2)
    };
  }
};
var MetadataFileParsedCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.record != null) {
      MetadataFileRecordCodec.write(s.initStruct(0, 2, 14), v.record, caps);
    }
  },
  read(s, caps) {
    return {
      record: MetadataFileRecordCodec.read(s.getStruct(0, 2, 14), caps)
    };
  }
};
var MetadataFileParseReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MetadataFileParsedCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MetadataFileParseReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MetadataFileParsedCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MetadataFileParseReply", disc);
    }
  }
};
var MetadataAudioFileCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint64(0, BigInt(v.durationMs ?? 0));
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      durationMs: Number(s.getUint64(0))
    };
  }
};
var MetadataFileRenderParamsCodec = {
  dataWords: 0,
  pointerCount: 5,
  write(s, v, caps) {
    s.setText(0, v.dialect ?? "");
    if (v.record != null) {
      MetadataFileRecordCodec.write(s.initStruct(1, 2, 14), v.record, caps);
    }
    s.setUint16List(2, (v.fields ?? []).map((v2) => ord(METADATA_FIELDS, v2, "MetadataField")));
    s.setData(3, v.existing ?? EMPTY_BYTES);
    {
      const list = v.audioFiles ?? [];
      const items = s.initStructList(4, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        MetadataAudioFileCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      dialect: s.getText(0),
      record: MetadataFileRecordCodec.read(s.getStruct(1, 2, 14), caps),
      fields: s.getUint16List(2).map((v) => fromOrd(METADATA_FIELDS, v, "MetadataField")),
      existing: s.getData(3),
      audioFiles: s.getStructList(4, 1, 1).map((item) => MetadataAudioFileCodec.read(item, caps))
    };
  }
};
var MetadataFileRenderedCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setData(0, v.bytes ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    return {
      bytes: s.getData(0)
    };
  }
};
var MetadataFileRenderReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          MetadataFileRenderedCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("MetadataFileRenderReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: MetadataFileRenderedCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("MetadataFileRenderReply", disc);
    }
  }
};
var NotificationFieldCodec = {
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
var NotificationMessageCodec = {
  dataWords: 1,
  pointerCount: 10,
  write(s, v, caps) {
    s.setText(0, v.notificationId ?? "");
    s.setText(1, v.notificationType ?? "");
    s.setUint16(0, ord(NOTIFICATION_SEVERITYS, v.severity ?? NOTIFICATION_SEVERITYS[0], "NotificationSeverity"));
    s.setText(2, v.title ?? "");
    s.setText(3, v.text ?? "");
    if (v.markdown !== void 0) {
      s.setText(4, v.markdown ?? "");
    }
    if (v.html !== void 0) {
      s.setText(5, v.html ?? "");
    }
    if (v.link !== void 0) {
      s.setText(6, v.link ?? "");
    }
    {
      const list = v.fields ?? [];
      const items = s.initStructList(7, list.length, 0, 2);
      for (let i = 0; i < items.length; i++) {
        NotificationFieldCodec.write(items[i], list[i], caps);
      }
    }
    if (v.groupKey !== void 0) {
      s.setText(8, v.groupKey ?? "");
    }
    s.setUint16(1, ord(NOTIFICATION_ALERT_ACTIONS, v.alertAction ?? NOTIFICATION_ALERT_ACTIONS[0], "NotificationAlertAction"));
    if (v.alertKey !== void 0) {
      s.setText(9, v.alertKey ?? "");
    }
    s.setBool(32, v.sensitive ?? false);
  },
  read(s, caps) {
    const out = {
      notificationId: s.getText(0),
      notificationType: s.getText(1),
      severity: fromOrd(NOTIFICATION_SEVERITYS, s.getUint16(0), "NotificationSeverity"),
      title: s.getText(2),
      text: s.getText(3),
      fields: s.getStructList(7, 0, 2).map((item) => NotificationFieldCodec.read(item, caps)),
      alertAction: fromOrd(NOTIFICATION_ALERT_ACTIONS, s.getUint16(1), "NotificationAlertAction"),
      sensitive: s.getBool(32)
    };
    const markdownValue = s.getText(4);
    if (!(markdownValue === "")) {
      out.markdown = markdownValue;
    }
    const htmlValue = s.getText(5);
    if (!(htmlValue === "")) {
      out.html = htmlValue;
    }
    const linkValue = s.getText(6);
    if (!(linkValue === "")) {
      out.link = linkValue;
    }
    const groupKeyValue = s.getText(8);
    if (!(groupKeyValue === "")) {
      out.groupKey = groupKeyValue;
    }
    const alertKeyValue = s.getText(9);
    if (!(alertKeyValue === "")) {
      out.alertKey = alertKeyValue;
    }
    return out;
  }
};
var NotifyDestinationCodec = {
  dataWords: 1,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setUint16(0, ord(NOTIFY_DESTINATION_KINDS, v.kind ?? NOTIFY_DESTINATION_KINDS[0], "NotifyDestinationKind"));
    s.setText(0, v.destinationId ?? "");
    s.setText(1, v.addressJson ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      kind: fromOrd(NOTIFY_DESTINATION_KINDS, s.getUint16(0), "NotifyDestinationKind"),
      destinationId: s.getText(0),
      addressJson: s.getText(1)
    };
  }
};
var NotifySendParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.message != null) {
      NotificationMessageCodec.write(s.initStruct(0, 1, 10), v.message, caps);
    }
    if (v.destination != null) {
      NotifyDestinationCodec.write(s.initStruct(1, 1, 2), v.destination, caps);
    }
  },
  read(s, caps) {
    return {
      message: NotificationMessageCodec.read(s.getStruct(0, 1, 10), caps),
      destination: NotifyDestinationCodec.read(s.getStruct(1, 1, 2), caps)
    };
  }
};
var NotifySentCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    if (v.providerId !== void 0) {
      s.setText(0, v.providerId ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {};
    const providerIdValue = s.getText(0);
    if (!(providerIdValue === "")) {
      out.providerId = providerIdValue;
    }
    return out;
  }
};
var NotifySendReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          NotifySentCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("NotifySendReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: NotifySentCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("NotifySendReply", disc);
    }
  }
};
var NotifyValidateParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.addressJson ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      addressJson: s.getText(0)
    };
  }
};
var NotifyValidatedCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.addressJson ?? "");
    if (v.label !== void 0) {
      s.setText(1, v.label ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      addressJson: s.getText(0)
    };
    const labelValue = s.getText(1);
    if (!(labelValue === "")) {
      out.label = labelValue;
    }
    return out;
  }
};
var NotifyValidateReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          NotifyValidatedCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("NotifyValidateReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: NotifyValidatedCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("NotifyValidateReply", disc);
    }
  }
};
var AiModelCodec = {
  dataWords: 2,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.modelId ?? "");
    s.setUint16(0, ord(AI_TASKS, v.task ?? AI_TASKS[0], "AiTask"));
    s.setText(1, v.revision ?? "");
    if (v.dimensions !== void 0) {
      s.setUint32(1, v.dimensions ?? 0);
    }
    if (v.maxTokens !== void 0) {
      s.setUint32(2, v.maxTokens ?? 0);
    }
    s.setTextList(2, v.languages ?? []);
  },
  read(s, caps) {
    void caps;
    const out = {
      modelId: s.getText(0),
      task: fromOrd(AI_TASKS, s.getUint16(0), "AiTask"),
      revision: s.getText(1),
      languages: s.getTextList(2)
    };
    const dimensionsValue = s.getUint32(1);
    if (!(dimensionsValue === 0)) {
      out.dimensions = dimensionsValue;
    }
    const maxTokensValue = s.getUint32(2);
    if (!(maxTokensValue === 0)) {
      out.maxTokens = maxTokensValue;
    }
    return out;
  }
};
var AiModelListCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.models ?? [];
      const items = s.initStructList(0, list.length, 2, 3);
      for (let i = 0; i < items.length; i++) {
        AiModelCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      models: s.getStructList(0, 2, 3).map((item) => AiModelCodec.read(item, caps))
    };
  }
};
var AiModelsReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          AiModelListCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("AiModelsReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: AiModelListCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("AiModelsReply", disc);
    }
  }
};
var AiEmbedTextCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.text ?? "");
    s.setUint16(0, ord(AI_TEXT_ROLES, v.role ?? AI_TEXT_ROLES[0], "AiTextRole"));
  },
  read(s, caps) {
    void caps;
    return {
      text: s.getText(0),
      role: fromOrd(AI_TEXT_ROLES, s.getUint16(0), "AiTextRole")
    };
  }
};
var AiTextEmbeddingsInputCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    {
      const list = v.texts ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        AiEmbedTextCodec.write(items[i], list[i], caps);
      }
    }
  },
  read(s, caps) {
    return {
      texts: s.getStructList(0, 1, 1).map((item) => AiEmbedTextCodec.read(item, caps))
    };
  }
};
var AiTextEmbeddingsOutputCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setUint32(0, v.dimensions ?? 0);
    s.setData(0, v.vectors ?? EMPTY_BYTES);
    s.setText(1, v.revision ?? "");
    s.setUint32List(2, v.fallbacks ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      dimensions: s.getUint32(0),
      vectors: s.getData(0),
      revision: s.getText(1),
      fallbacks: s.getUint32List(2)
    };
  }
};
var AiInputCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.textEmbeddings != null) {
      AiTextEmbeddingsInputCodec.write(s.initStruct(0, 0, 1), v.textEmbeddings, caps);
    }
  },
  read(s, caps) {
    return {
      textEmbeddings: AiTextEmbeddingsInputCodec.read(s.getStruct(0, 0, 1), caps)
    };
  }
};
var AiOutputCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.textEmbeddings != null) {
      AiTextEmbeddingsOutputCodec.write(s.initStruct(0, 1, 3), v.textEmbeddings, caps);
    }
  },
  read(s, caps) {
    return {
      textEmbeddings: AiTextEmbeddingsOutputCodec.read(s.getStruct(0, 1, 3), caps)
    };
  }
};
var AiRunParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    s.setText(0, v.model ?? "");
    if (v.input != null) {
      AiInputCodec.write(s.initStruct(1, 0, 1), v.input, caps);
    }
  },
  read(s, caps) {
    return {
      model: s.getText(0),
      input: AiInputCodec.read(s.getStruct(1, 0, 1), caps)
    };
  }
};
var AiRunOkCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.output != null) {
      AiOutputCodec.write(s.initStruct(0, 0, 1), v.output, caps);
    }
    if (v.model !== void 0) {
      s.setText(1, v.model ?? "");
    }
  },
  read(s, caps) {
    const out = {
      output: AiOutputCodec.read(s.getStruct(0, 0, 1), caps)
    };
    const modelValue = s.getText(1);
    if (!(modelValue === "")) {
      out.model = modelValue;
    }
    return out;
  }
};
var AiRunReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          AiRunOkCodec.write(s.initStruct(0, 0, 2), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("AiRunReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: AiRunOkCodec.read(s.getStruct(0, 0, 2), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("AiRunReply", disc);
    }
  }
};
var SocketFrameCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    switch (v.kind) {
      case "text":
        s.setUint16(0, 0);
        s.setText(0, v.value ?? "");
        break;
      case "binary":
        s.setUint16(0, 1);
        s.setData(0, v.value ?? EMPTY_BYTES);
        break;
      default:
        throw unknownUnion("SocketFrame", v.kind);
    }
  },
  read(s, caps) {
    void caps;
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "text", value: s.getText(0) };
      case 1:
        return { kind: "binary", value: s.getData(0) };
      default:
        throw unknownUnion("SocketFrame", disc);
    }
  }
};
var SocketMessageReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          SocketMessageOkCodec.write(s.initStruct(0, 1, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("SocketMessageReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: SocketMessageOkCodec.read(s.getStruct(0, 1, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("SocketMessageReply", disc);
    }
  }
};
var SocketMessageOkCodec = {
  dataWords: 1,
  pointerCount: 0,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.pollEvents ?? false);
  },
  read(s, caps) {
    void caps;
    return {
      pollEvents: s.getBool(0)
    };
  }
};
var SocketMessageCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.socket ?? "");
    if (v.frame != null) {
      SocketFrameCodec.write(s.initStruct(1, 1, 1), v.frame, caps);
    }
    s.setData(2, v.attachment ?? EMPTY_BYTES);
  },
  read(s, caps) {
    return {
      socket: s.getText(0),
      frame: SocketFrameCodec.read(s.getStruct(1, 1, 1), caps),
      attachment: s.getData(2)
    };
  }
};
var SocketClosedCodec = {
  dataWords: 1,
  pointerCount: 3,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.socket ?? "");
    s.setUint16(0, v.code ?? 0);
    if (v.reason !== void 0) {
      s.setText(1, v.reason ?? "");
    }
    s.setBool(16, v.clean ?? false);
    s.setData(2, v.attachment ?? EMPTY_BYTES);
  },
  read(s, caps) {
    void caps;
    const out = {
      socket: s.getText(0),
      code: s.getUint16(0),
      clean: s.getBool(16),
      attachment: s.getData(2)
    };
    const reasonValue = s.getText(1);
    if (!(reasonValue === "")) {
      out.reason = reasonValue;
    }
    return out;
  }
};
var IngressStartedCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.origin ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      origin: s.getText(0)
    };
  }
};
var IngressStartReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          IngressStartedCodec.write(s.initStruct(0, 0, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("IngressStartReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: IngressStartedCodec.read(s.getStruct(0, 0, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("IngressStartReply", disc);
    }
  }
};
var IngressHealthCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setBool(0, v.serving ?? false);
    if (v.detail !== void 0) {
      s.setText(0, v.detail ?? "");
    }
  },
  read(s, caps) {
    void caps;
    const out = {
      serving: s.getBool(0)
    };
    const detailValue = s.getText(0);
    if (!(detailValue === "")) {
      out.detail = detailValue;
    }
    return out;
  }
};
var IngressHealthReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "ok":
        s.setUint16(0, 0);
        if (v.value != null) {
          IngressHealthCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("IngressHealthReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "ok", value: IngressHealthCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("IngressHealthReply", disc);
    }
  }
};
var WebhookDeliveredCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.name ?? "");
    s.setUint32(0, v.waiting ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      name: s.getText(0),
      waiting: s.getUint32(0)
    };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("ExecuteResultReply", disc);
    }
  }
};
var ExecuteStreamHeadCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.operationId ?? "");
    s.setUint32(0, v.statementCount ?? 0);
  },
  read(s, caps) {
    void caps;
    return {
      operationId: s.getText(0),
      statementCount: s.getUint32(0)
    };
  }
};
var ExecuteStreamStatementCodec = {
  dataWords: 2,
  pointerCount: 1,
  write(s, v, caps) {
    s.setUint32(0, v.index ?? 0);
    {
      const list = v.columns ?? [];
      const items = s.initStructList(0, list.length, 1, 1);
      for (let i = 0; i < items.length; i++) {
        DbColumnCodec.write(items[i], list[i], caps);
      }
    }
    s.setUint64(1, BigInt(v.rowsAffected ?? 0));
  },
  read(s, caps) {
    return {
      index: s.getUint32(0),
      columns: s.getStructList(0, 1, 1).map((item) => DbColumnCodec.read(item, caps)),
      rowsAffected: Number(s.getUint64(1))
    };
  }
};
var ExecuteStreamFrameCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "head":
        s.setUint16(0, 0);
        if (v.value != null) {
          ExecuteStreamHeadCodec.write(s.initStruct(0, 1, 1), v.value, caps);
        }
        break;
      case "statement":
        s.setUint16(0, 1);
        if (v.value != null) {
          ExecuteStreamStatementCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      case "rows":
        s.setUint16(0, 2);
        {
          const list = v.value ?? [];
          const items = s.initStructList(0, list.length, 0, 1);
          for (let i = 0; i < items.length; i++) {
            DbRowCodec.write(items[i], list[i], caps);
          }
        }
        break;
      case "end":
        s.setUint16(0, 3);
        if (v.value != null) {
          DbTimingCodec.write(s.initStruct(0, 2, 1), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 4);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExecuteStreamFrame", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "head", value: ExecuteStreamHeadCodec.read(s.getStruct(0, 1, 1), caps) };
      case 1:
        return { kind: "statement", value: ExecuteStreamStatementCodec.read(s.getStruct(0, 2, 1), caps) };
      case 2:
        return { kind: "rows", value: s.getStructList(0, 0, 1).map((item) => DbRowCodec.read(item, caps)) };
      case 3:
        return { kind: "end", value: DbTimingCodec.read(s.getStruct(0, 2, 1), caps) };
      case 4:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("ExecuteStreamFrame", disc);
    }
  }
};
var ExecuteStreamReplyCodec = {
  dataWords: 1,
  pointerCount: 1,
  write(s, v, caps) {
    switch (v.kind) {
      case "inline":
        s.setUint16(0, 0);
        s.setData(0, v.value ?? EMPTY_BYTES);
        break;
      case "stream":
        s.setUint16(0, 1);
        if (v.value != null) {
          s.setCap(0, caps.exportCap(v.value));
        }
        break;
      case "err":
        s.setUint16(0, 2);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
        }
        break;
      default:
        throw unknownUnion("ExecuteStreamReply", v.kind);
    }
  },
  read(s, caps) {
    const disc = s.getUint16(0);
    switch (disc) {
      case 0:
        return { kind: "inline", value: s.getData(0) };
      case 1:
        return { kind: "stream", value: caps.importCap(s.getCapIndex(0)) };
      case 2:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
      default:
        throw unknownUnion("ExecuteStreamReply", disc);
    }
  }
};
var DbCapabilitiesCodec = {
  dataWords: 8,
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
    s.setUint32(14, v.maxStreamedResultBytes ?? 0);
    s.setUint32(15, v.maxStreamedResultRows ?? 0);
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
      atomicUnitRestore: s.getBool(40),
      maxStreamedResultBytes: s.getUint32(14),
      maxStreamedResultRows: s.getUint32(15)
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          DbCapabilitiesCodec.write(s.initStruct(0, 8, 0), v.value, caps);
        }
        break;
      case "err":
        s.setUint16(0, 1);
        if (v.value != null) {
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "ok", value: DbCapabilitiesCodec.read(s.getStruct(0, 8, 0), caps) };
      case 1:
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
          PluginErrorCodec.write(s.initStruct(0, 0, 3), v.value, caps);
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
        return { kind: "err", value: PluginErrorCodec.read(s.getStruct(0, 0, 3), caps) };
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
      ListOptionsCodec.write(s.initStruct(0, 2, 2), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      options: ListOptionsCodec.read(s.getStruct(0, 2, 2), caps)
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
var DestinationMoveParamsCodec = {
  dataWords: 0,
  pointerCount: 3,
  write(s, v, caps) {
    s.setText(0, v.from ?? "");
    s.setText(1, v.to ?? "");
    if (v.options != null) {
      MoveOptionsCodec.write(s.initStruct(2, 1, 1), v.options, caps);
    }
  },
  read(s, caps) {
    return {
      from: s.getText(0),
      to: s.getText(1),
      options: MoveOptionsCodec.read(s.getStruct(2, 1, 1), caps)
    };
  }
};
var DestinationMoveResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MoveReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MoveReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationMoveManyParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.request != null) {
      MoveManyRequestCodec.write(s.initStruct(0, 1, 3), v.request, caps);
    }
  },
  read(s, caps) {
    return {
      request: MoveManyRequestCodec.read(s.getStruct(0, 1, 3), caps)
    };
  }
};
var DestinationMoveManyResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MoveManyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MoveManyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationDeleteManyParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.keys ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      keys: s.getTextList(0)
    };
  }
};
var DestinationDeleteManyResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      DeleteManyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: DeleteManyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationHeadManyParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.keys ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      keys: s.getTextList(0)
    };
  }
};
var DestinationHeadManyResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      HeadManyReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: HeadManyReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationChangesParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.request != null) {
      ChangesRequestCodec.write(s.initStruct(0, 1, 1), v.request, caps);
    }
  },
  read(s, caps) {
    return {
      request: ChangesRequestCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationChangesResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ChangesReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ChangesReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var DestinationTranslateKeysParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setTextList(0, v.keys ?? []);
  },
  read(s, caps) {
    void caps;
    return {
      keys: s.getTextList(0)
    };
  }
};
var DestinationTranslateKeysResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      TranslateKeysReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: TranslateKeysReplyCodec.read(s.getStruct(0, 1, 1), caps)
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
var ScheduledHandlerScheduledParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.controller != null) {
      ScheduledControllerCodec.write(s.initStruct(0, 3, 2), v.controller, caps);
    }
  },
  read(s, caps) {
    return {
      controller: ScheduledControllerCodec.read(s.getStruct(0, 3, 2), caps)
    };
  }
};
var ScheduledHandlerScheduledResultsCodec = {
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
var QueueConsumerQueueParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.batch != null) {
      QueueBatchCodec.write(s.initStruct(0, 0, 2), v.batch, caps);
    }
  },
  read(s, caps) {
    return {
      batch: QueueBatchCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var QueueConsumerQueueResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      QueueBatchReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: QueueBatchReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var SocketHandlerMessageParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.event != null) {
      SocketMessageCodec.write(s.initStruct(0, 0, 3), v.event, caps);
    }
  },
  read(s, caps) {
    return {
      event: SocketMessageCodec.read(s.getStruct(0, 0, 3), caps)
    };
  }
};
var SocketHandlerMessageResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      SocketMessageReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: SocketMessageReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var SocketHandlerClosedParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.event != null) {
      SocketClosedCodec.write(s.initStruct(0, 1, 3), v.event, caps);
    }
  },
  read(s, caps) {
    return {
      event: SocketClosedCodec.read(s.getStruct(0, 1, 3), caps)
    };
  }
};
var SocketHandlerClosedResultsCodec = {
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
var IngressStartParamsCodec = {
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
var IngressStartResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      IngressStartReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: IngressStartReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var IngressHealthParamsCodec = {
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
var IngressHealthResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      IngressHealthReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: IngressHealthReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var IngressStopParamsCodec = {
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
var IngressStopResultsCodec = {
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
var WebhookHandlerDeliveredParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.event != null) {
      WebhookDeliveredCodec.write(s.initStruct(0, 1, 1), v.event, caps);
    }
  },
  read(s, caps) {
    return {
      event: WebhookDeliveredCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var WebhookHandlerDeliveredResultsCodec = {
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
      ScanParamsCodec.write(s.initStruct(0, 1, 5), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: ScanParamsCodec.read(s.getStruct(0, 1, 5), caps)
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
var ContentSourceRevokeAccountParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      RevokeAccountParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: RevokeAccountParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var ContentSourceRevokeAccountResultsCodec = {
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
var ContentSourcePurchaseHintsParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      PurchaseHintsParamsCodec.write(s.initStruct(0, 0, 1), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: PurchaseHintsParamsCodec.read(s.getStruct(0, 0, 1), caps)
    };
  }
};
var ContentSourcePurchaseHintsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      PurchaseHintsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: PurchaseHintsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var ContentSourceQualityOffersParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      QualityOffersParamsCodec.write(s.initStruct(0, 0, 5), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: QualityOffersParamsCodec.read(s.getStruct(0, 0, 5), caps)
    };
  }
};
var ContentSourceQualityOffersResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      QualityOffersReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: QualityOffersReplyCodec.read(s.getStruct(0, 1, 1), caps)
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
var MetadataProviderHealthParamsCodec = {
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
var MetadataProviderHealthResultsCodec = {
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
var MetadataProviderLookupParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      MetadataLookupParamsCodec.write(s.initStruct(0, 1, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: MetadataLookupParamsCodec.read(s.getStruct(0, 1, 2), caps)
    };
  }
};
var MetadataProviderLookupResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MetadataRecordsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MetadataRecordsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var MetadataProviderSearchParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      MetadataSearchParamsCodec.write(s.initStruct(0, 1, 4), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: MetadataSearchParamsCodec.read(s.getStruct(0, 1, 4), caps)
    };
  }
};
var MetadataProviderSearchResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MetadataRecordsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MetadataRecordsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var MetadataProviderChaptersParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      MetadataChaptersParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: MetadataChaptersParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var MetadataProviderChaptersResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MetadataChaptersReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MetadataChaptersReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var CoverFetcherHealthParamsCodec = {
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
var CoverFetcherHealthResultsCodec = {
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
var CoverFetcherFetchCoversParamsCodec = {
  dataWords: 0,
  pointerCount: 2,
  write(s, v, caps) {
    if (v.params != null) {
      CoverFetchParamsCodec.write(s.initStruct(0, 1, 2), v.params, caps);
    }
    if (v.sink != null) {
      s.setCap(1, caps.exportCap(v.sink));
    }
  },
  read(s, caps) {
    const out = {
      params: CoverFetchParamsCodec.read(s.getStruct(0, 1, 2), caps)
    };
    const sinkValue = caps.importCap(s.getCapIndex(1));
    if (!(sinkValue == null)) {
      out.sink = sinkValue;
    }
    return out;
  }
};
var CoverFetcherFetchCoversResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      CoverImagesReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: CoverImagesReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var MetadataFilesHealthParamsCodec = {
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
var MetadataFilesHealthResultsCodec = {
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
var MetadataFilesParseParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      MetadataFileParseParamsCodec.write(s.initStruct(0, 0, 3), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: MetadataFileParseParamsCodec.read(s.getStruct(0, 0, 3), caps)
    };
  }
};
var MetadataFilesParseResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MetadataFileParseReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MetadataFileParseReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var MetadataFilesRenderParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      MetadataFileRenderParamsCodec.write(s.initStruct(0, 0, 5), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: MetadataFileRenderParamsCodec.read(s.getStruct(0, 0, 5), caps)
    };
  }
};
var MetadataFilesRenderResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      MetadataFileRenderReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: MetadataFileRenderReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var NotifierHealthParamsCodec = {
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
var NotifierHealthResultsCodec = {
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
var NotifierSendParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      NotifySendParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: NotifySendParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var NotifierSendResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      NotifySendReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: NotifySendReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var NotifierValidateDestinationParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      NotifyValidateParamsCodec.write(s.initStruct(0, 0, 1), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: NotifyValidateParamsCodec.read(s.getStruct(0, 0, 1), caps)
    };
  }
};
var NotifierValidateDestinationResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      NotifyValidateReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: NotifyValidateReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AiModelsHealthParamsCodec = {
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
var AiModelsHealthResultsCodec = {
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
var AiModelsModelsParamsCodec = {
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
var AiModelsModelsResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      AiModelsReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: AiModelsReplyCodec.read(s.getStruct(0, 1, 1), caps)
    };
  }
};
var AiModelsRunParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.params != null) {
      AiRunParamsCodec.write(s.initStruct(0, 0, 2), v.params, caps);
    }
  },
  read(s, caps) {
    return {
      params: AiRunParamsCodec.read(s.getStruct(0, 0, 2), caps)
    };
  }
};
var AiModelsRunResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      AiRunReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: AiRunReplyCodec.read(s.getStruct(0, 1, 1), caps)
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
var DatabaseDropUnitParamsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    void caps;
    s.setText(0, v.unitRef ?? "");
  },
  read(s, caps) {
    void caps;
    return {
      unitRef: s.getText(0)
    };
  }
};
var DatabaseDropUnitResultsCodec = {
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
var AdapterDatabaseSessionExecuteStreamedParamsCodec = {
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
var AdapterDatabaseSessionExecuteStreamedResultsCodec = {
  dataWords: 0,
  pointerCount: 1,
  write(s, v, caps) {
    if (v.result != null) {
      ExecuteStreamReplyCodec.write(s.initStruct(0, 1, 1), v.result, caps);
    }
  },
  read(s, caps) {
    return {
      result: ExecuteStreamReplyCodec.read(s.getStruct(0, 1, 1), caps)
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

// dist/db-stream.js
var ROWS_FRAME = 2;
function streamsResults(caps) {
  return caps.maxStreamedResultBytes > 0 && caps.maxStreamedResultRows > 0;
}
function executeStreamLimits(caps) {
  if (streamsResults(caps)) {
    return {
      maxBytes: caps.maxStreamedResultBytes,
      bytesLimit: "maxStreamedResultBytes",
      maxRows: caps.maxStreamedResultRows,
      rowsLimit: "maxStreamedResultRows"
    };
  }
  return {
    maxBytes: caps.maxAtomicResultBytes,
    bytesLimit: "maxAtomicResultBytes",
    maxRows: caps.maxResultRows,
    rowsLimit: "maxResultRows"
  };
}
function rawStreamCeiling(limits) {
  return limits.maxBytes + Math.floor(limits.maxBytes / 8) + MAX_STREAM_WINDOW_BYTES;
}
var ExecuteStreamDecoder = class {
  #limits;
  #retainRows;
  /** Bytes not yet consumed: `#buf[0, #len)`; the rest is spare capacity. */
  #buf = new Uint8Array(0);
  /** Bytes of `#buf` in use. */
  #len = 0;
  #seen = 0;
  #state = { kind: "awaitingHead" };
  #columns = [];
  /**
   * @param limits - Budget the stream is held to.
   * @param options - `retainRows: false` counts rows against the budget but
   *   drops them from the collected reply (for `iterate()`).
   */
  constructor(limits, options = {}) {
    this.#limits = limits;
    this.#retainRows = options.retainRows ?? true;
  }
  /**
   * Whether the final frame (`end` or `err`) has been decoded.
   *
   * @returns True once the stream is complete.
   */
  get finished() {
    return this.#state.kind === "finished";
  }
  /**
   * Columns of the statement whose rows are arriving.
   *
   * @returns The open statement's columns, or an empty list between statements.
   */
  get columns() {
    return this.#columns;
  }
  /**
   * Feeds the next bytes of the stream.
   *
   * @param bytes - Any slice of the frame sequence.
   * @returns The frames completed by these bytes, in order.
   * @throws `PluginError` `limit_exceeded` when the stream exceeds the byte
   *   or row budget, `invalid_params` when a frame is malformed or out of
   *   sequence or bytes follow the final frame.
   */
  push(bytes) {
    try {
      return this.#push(bytes);
    } catch (err) {
      if (err instanceof PluginError) {
        this.#columns = [];
        this.#state = { kind: "finished", outcome: { err } };
      }
      throw err;
    }
  }
  /**
   * `push` before its failure is latched.
   *
   * @param bytes - Any slice of the frame sequence.
   * @returns The frames completed by these bytes, in order.
   * @throws See {@link ExecuteStreamDecoder.push}.
   */
  #push(bytes) {
    const frames = [];
    if (bytes.byteLength === 0) {
      return frames;
    }
    if (this.finished) {
      throw PluginError.fromWire("invalid_params", "streamed reply carries bytes after its final frame");
    }
    this.#seen += bytes.byteLength;
    const ceiling = rawStreamCeiling(this.#limits);
    if (this.#seen > ceiling) {
      throw this.#bytesExceeded(this.#seen);
    }
    this.#append(bytes);
    let at = 0;
    while (this.#len - at >= 8) {
      const view = new DataView(this.#buf.buffer, this.#buf.byteOffset + at, 8);
      if (view.getUint32(0, true) !== 0) {
        throw PluginError.fromWire("invalid_params", "streamed reply frame is not a single-segment Cap'n message");
      }
      const frameLen = view.getUint32(4, true) * 8 + 8;
      if (frameLen > ceiling) {
        throw this.#bytesExceeded(frameLen);
      }
      if (this.#len - at < frameLen) {
        break;
      }
      const frame2 = decodeFrame(this.#buf.slice(at, at + frameLen), ceiling, (count) => this.#admitRows(count));
      at += frameLen;
      this.#apply(frame2);
      frames.push(frame2);
      if (this.finished) {
        if (this.#len > at) {
          throw PluginError.fromWire("invalid_params", "streamed reply carries bytes after its final frame");
        }
        break;
      }
    }
    this.#consume(at);
    return frames;
  }
  /**
   * Appends `bytes` to the unconsumed remainder.
   *
   * The backing store grows geometrically, so a frame that arrives in many
   * small chunks is copied a bounded number of times rather than once per
   * chunk. A push onto an empty remainder is held as is, without a copy.
   *
   * @param bytes - The pushed bytes.
   */
  #append(bytes) {
    if (this.#len === 0) {
      this.#buf = bytes;
      this.#len = bytes.byteLength;
      return;
    }
    const need = this.#len + bytes.byteLength;
    if (need > this.#buf.byteLength) {
      const grown = new Uint8Array(Math.max(need, this.#buf.byteLength * 2));
      grown.set(this.#buf.subarray(0, this.#len), 0);
      this.#buf = grown;
    }
    this.#buf.set(bytes, this.#len);
    this.#len = need;
  }
  /**
   * Drops the first `at` bytes of the remainder.
   *
   * A remainder consumed whole releases its backing store, so a completed
   * decoder an author keeps does not pin the last chunk it was pushed.
   *
   * @param at - Bytes consumed from the front.
   */
  #consume(at) {
    if (at === 0) {
      return;
    }
    if (at === this.#len) {
      this.#buf = new Uint8Array(0);
      this.#len = 0;
      return;
    }
    const rest = this.#buf.slice(at, this.#len);
    this.#buf = rest;
    this.#len = rest.byteLength;
  }
  /**
   * Takes the reassembled reply once the stream has ended.
   *
   * @returns The reply the frames described.
   * @throws The adapter's `PluginError` when the stream ended with `err`, and
   *   `unavailable` when it ended before its final frame.
   */
  finish() {
    if (this.#state.kind !== "finished") {
      throw PluginError.fromWire("unavailable", "streamed reply ended before its final frame");
    }
    const outcome = this.#state.outcome;
    if ("err" in outcome) {
      throw outcome.err;
    }
    return outcome.ok;
  }
  /**
   * The typed error for a stream over its byte budget.
   *
   * @param used - Bytes seen or declared.
   * @returns `limit_exceeded` naming the budget's capability field.
   */
  #bytesExceeded(used) {
    return PluginError.limitExceeded({
      limit: this.#limits.bytesLimit,
      cap: this.#limits.maxBytes,
      used
    });
  }
  /**
   * The error for a frame out of sequence.
   *
   * @param message - What the stream did, completing "streamed reply …".
   * @returns `invalid_params`.
   */
  #sequence(message) {
    return PluginError.fromWire("invalid_params", `streamed reply ${message}`);
  }
  /**
   * Admits a rows frame by its row count before any of its cells decode, so
   * a frame past the row cap (or out of sequence) is refused without
   * materializing the rows it carries.
   *
   * @param count - Rows the frame announces.
   * @throws `PluginError` `invalid_params` when no statement is open,
   *   `limit_exceeded` when the rows would pass the statement's row cap.
   */
  #admitRows(count) {
    const state = this.#state;
    if (state.kind !== "inReply" || !state.open) {
      throw this.#sequence("carries rows before a statement frame");
    }
    const used = state.openRows + count;
    if (this.#limits.maxRows > 0 && used > this.#limits.maxRows) {
      throw PluginError.limitExceeded({
        limit: this.#limits.rowsLimit,
        cap: this.#limits.maxRows,
        used
      });
    }
  }
  /**
   * Advances the sequence by one decoded frame: opens the reply on `head`,
   * a statement on `statement`, counts (and keeps, when retained) `rows`,
   * and closes the reply on `end` or `err`.
   *
   * @param frame - The frame just decoded.
   * @throws `PluginError` `invalid_params` when the frame is out of sequence
   *   or a cell is not a valid `DbValue`, `limit_exceeded` past the row cap.
   */
  #apply(frame2) {
    const state = this.#state;
    if (frame2.kind === "err") {
      this.#columns = [];
      this.#state = {
        kind: "finished",
        outcome: {
          err: PluginError.fromWire(frame2.value.code, frame2.value.message, frame2.value.details)
        }
      };
      return;
    }
    if (state.kind === "awaitingHead") {
      if (frame2.kind !== "head") {
        throw this.#sequence(`opens with a ${frame2.kind} frame instead of head`);
      }
      this.#state = {
        kind: "inReply",
        operationId: frame2.value.operationId,
        statementCount: frame2.value.statementCount,
        nextIndex: 0,
        statements: [],
        open: void 0,
        openRows: 0
      };
      return;
    }
    if (state.kind === "finished") {
      throw this.#sequence("carries a frame after its final frame");
    }
    switch (frame2.kind) {
      case "head":
        throw this.#sequence("carries a second head frame");
      case "statement": {
        if (frame2.value.index !== state.nextIndex) {
          throw this.#sequence(`statement frame ${frame2.value.index} arrived where ${state.nextIndex} was expected`);
        }
        if (state.nextIndex >= state.statementCount) {
          throw this.#sequence(`carries more statements than its head announced`);
        }
        if (state.open) {
          state.statements.push(state.open);
        }
        state.open = {
          rows: [],
          columns: frame2.value.columns,
          rowsAffected: frame2.value.rowsAffected
        };
        state.openRows = 0;
        state.nextIndex += 1;
        this.#columns = frame2.value.columns;
        return;
      }
      case "rows": {
        const open = state.open;
        if (!open) {
          throw this.#sequence("carries rows before a statement frame");
        }
        for (const row of frame2.value) {
          if (row.values.length !== open.columns.length) {
            throw this.#sequence(`carries a row of ${row.values.length} cells for ${open.columns.length} columns`);
          }
          for (const cell of row.values) {
            validateCell(cell);
          }
        }
        state.openRows += frame2.value.length;
        if (this.#limits.maxRows > 0 && state.openRows > this.#limits.maxRows) {
          throw PluginError.limitExceeded({
            limit: this.#limits.rowsLimit,
            cap: this.#limits.maxRows,
            used: state.openRows
          });
        }
        if (this.#retainRows) {
          for (const row of frame2.value) {
            open.rows.push(row);
          }
        }
        return;
      }
      case "end": {
        if (state.open) {
          state.statements.push(state.open);
          state.open = void 0;
        }
        if (state.statements.length !== state.statementCount) {
          throw this.#sequence(`ended with ${state.statements.length} statements; its head announced ${state.statementCount}`);
        }
        this.#columns = [];
        this.#state = {
          kind: "finished",
          outcome: {
            ok: {
              operationId: state.operationId,
              statements: state.statements,
              timing: frame2.value
            }
          }
        };
        return;
      }
    }
  }
};
function decodeExecuteStream(bytes, limits) {
  const decoder = new ExecuteStreamDecoder(limits);
  decoder.push(bytes);
  return decoder.finish();
}
async function collectExecuteStream(chunks, limits) {
  const decoder = new ExecuteStreamDecoder(limits);
  for await (const chunk of chunks) {
    decoder.push(chunk);
  }
  return decoder.finish();
}
async function* streamChunks(body) {
  if (!body) {
    return;
  }
  const reader = body.getReader();
  let drained = false;
  try {
    for (; ; ) {
      const { done, value } = await reader.read();
      if (done) {
        drained = true;
        return;
      }
      if (value) {
        yield value;
      }
    }
  } finally {
    if (!drained) {
      await reader.cancel().catch(() => void 0);
    }
    reader.releaseLock();
  }
}
function validateCell(cell) {
  if (cell.kind === "float64" && !Number.isFinite(cell.value)) {
    throw PluginError.fromWire("invalid_params", "streamed reply float64 cell is not finite");
  }
  if (cell.kind === "text" && cell.value.includes("\0")) {
    throw PluginError.fromWire("invalid_params", "streamed reply TEXT cell contains U+0000");
  }
}
function decodeFrame(bytes, ceiling, admitRows) {
  const words = Math.min(Math.floor(bytes.byteLength / 8) + 16, Math.floor(Math.max(ceiling, MAX_SCALAR_BYTES) / 8));
  try {
    const reader = new CapnpReader(bytes, words);
    const root = reader.root(ExecuteStreamFrameCodec.dataWords, ExecuteStreamFrameCodec.pointerCount);
    if (root.getUint16(0) === ROWS_FRAME) {
      admitRows(root.getStructListLength(0));
    }
    return ExecuteStreamFrameCodec.read(root, NO_CAPS);
  } catch (err) {
    if (err instanceof PluginError) {
      throw err;
    }
    throw PluginError.fromWire("invalid_params", `streamed reply frame does not decode: ${err instanceof Error ? err.message : String(err)}`);
  }
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
var MAX_WRITE_KEY_BYTES = 256;
var WriteChunksError = class extends PluginError {
  /** Statements committed before the failure; resume with `from: next`. */
  next;
  /**
   * @param next - Statements committed before the failure.
   * @param cause - The failed batch's error, whose code, message and details
   *   this error carries.
   */
  constructor(next, cause) {
    super(cause.wireCode, cause.message, cause.details);
    this.name = "WriteChunksError";
    this.next = next;
  }
};
async function writeChunksOperationId(key, offset) {
  const bytes = new TextEncoder().encode(`bookclerk-write-chunks\0${key}\0${String(offset)}`);
  let hex;
  if (globalThis.crypto?.subtle) {
    hex = hexBytes(new Uint8Array(await globalThis.crypto.subtle.digest("SHA-256", bytes)));
  } else {
    const { createHash } = await import("node:crypto");
    hex = createHash("sha256").update(bytes).digest("hex");
  }
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20, 32)}`;
}
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
function encodeExecuteReply(reply) {
  const msg = new CapnpMessage();
  const root = msg.initRoot(0, 3);
  writeExecuteReply(root, reply);
  return msg.finish();
}
function encodeExecuteResultReply(outcome) {
  const msg = new CapnpMessage();
  const root = msg.initRoot(1, 1);
  if ("ok" in outcome) {
    root.setUint16(0, 0);
    writeExecuteReply(root.initStruct(0, 0, 3), outcome.ok);
  } else {
    root.setUint16(0, 1);
    const err = root.initStruct(0, 0, 3);
    err.setText(0, outcome.err.code);
    err.setText(1, outcome.err.message);
    err.setText(2, outcome.err.details ?? "");
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
    const err = root.getStruct(0, 0, 3);
    throw PluginError.fromWire(err.getText(0), err.getText(1), err.getText(2));
  }
  throw new Error("unknown ExecuteResultReply union member");
}
function encodeDbCapabilitiesReply(outcome) {
  return encodeMessage(DbCapabilitiesReplyCodec, "ok" in outcome ? { kind: "ok", value: outcome.ok } : {
    kind: "err",
    value: {
      code: outcome.err.code,
      message: outcome.err.message,
      details: outcome.err.details ?? ""
    }
  });
}
function decodeDbCapabilitiesReply(bytes) {
  const reply = decodeMessage(DbCapabilitiesReplyCodec, bytes);
  if (reply.kind === "ok") {
    return reply.value;
  }
  throw PluginError.fromWire(reply.value.code, reply.value.message, reply.value.details);
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
  let cachedLimits;
  const sessionLimits = () => {
    if (cachedLimits === void 0) {
      try {
        cachedLimits = transport.limits === void 0 ? Promise.resolve(null) : transport.limits().catch(() => null);
      } catch {
        cachedLimits = Promise.resolve(null);
      }
    }
    return cachedLimits;
  };
  const requestFor = async (batch, retry) => {
    if (!Array.isArray(batch) || batch.length === 0) {
      throw new Error("execute statements must be non-empty");
    }
    const request = await executeRequestFromBatch(batch, options, retry);
    const encoded = encodeExecuteRequest(request);
    const cap = tighterCap(options.maxRequestBytes ?? 0, (await sessionLimits())?.maxRequestBytes ?? 0);
    if (cap > 0 && encoded.byteLength > cap) {
      throw PluginError.limitExceeded({
        limit: "maxRequestBytes",
        cap,
        used: encoded.byteLength
      });
    }
    return request;
  };
  const openStreamed = async (request) => {
    if (transport.executeStreamed === void 0) {
      return void 0;
    }
    const caps = await sessionLimits();
    if (caps === null || !streamsResults(caps)) {
      return void 0;
    }
    let body;
    try {
      body = await transport.executeStreamed(request);
    } catch (err) {
      if (err instanceof PluginError && err.wireCode === "unsupported") {
        return void 0;
      }
      throw err;
    }
    const chunks = body instanceof Uint8Array ? (async function* one() {
      yield body;
    })() : streamChunks(body);
    return { chunks, caps };
  };
  const runExecute = async (batch, retry) => {
    const request = await requestFor(batch, retry);
    const streamed = await openStreamed(request);
    if (streamed === void 0) {
      return replyFor(request, await transport.execute(request));
    }
    const decoder = new ExecuteStreamDecoder(executeStreamLimits(streamed.caps));
    for await (const chunk of streamed.chunks) {
      decoder.push(chunk);
    }
    let reply;
    try {
      reply = decoder.finish();
    } catch (err) {
      if (err instanceof PluginError && err.wireCode === "unsupported") {
        return replyFor(request, await transport.execute(request));
      }
      throw err;
    }
    return replyFor(request, reply);
  };
  const inlineRows = async function* (request) {
    const reply = replyFor(request, await transport.execute(request));
    const result = reply.statements[0];
    if (result) {
      for (const row of result.rows) {
        yield rowMap(result.columns, row);
      }
    }
  };
  const iterateRows = async function* (batch, retry) {
    const request = await requestFor(batch, retry);
    const streamed = await openStreamed(request);
    if (streamed === void 0) {
      yield* inlineRows(request);
      return;
    }
    const decoder = new ExecuteStreamDecoder(executeStreamLimits(streamed.caps), {
      retainRows: false
    });
    const fallsBackInline = (err, yielded2) => !yielded2 && err instanceof PluginError && err.wireCode === "unsupported";
    let columns = [];
    let yielded = false;
    for await (const chunk of streamed.chunks) {
      let frames;
      try {
        frames = decoder.push(chunk);
      } catch (err) {
        if (fallsBackInline(err, yielded)) {
          yield* inlineRows(request);
          return;
        }
        throw err;
      }
      for (const frame2 of frames) {
        if (frame2.kind === "head") {
          checkReplyIdentity(request, frame2.value.operationId, frame2.value.statementCount);
        } else if (frame2.kind === "statement") {
          columns = frame2.value.columns;
        } else if (frame2.kind === "rows") {
          for (const row of frame2.value) {
            yielded = true;
            yield rowMap(columns, row);
          }
        }
      }
    }
    try {
      decoder.finish();
    } catch (err) {
      if (fallsBackInline(err, yielded)) {
        yield* inlineRows(request);
        return;
      }
      throw err;
    }
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
    },
    limits() {
      if (transport.limits === void 0) {
        return Promise.reject(PluginError.fromWire("unsupported", "GuestDatabase.limits is not available on this host"));
      }
      return sessionLimits().then(async (caps) => {
        if (caps !== null) {
          return caps;
        }
        const fetched = await transport.limits();
        cachedLimits = Promise.resolve(fetched);
        return fetched;
      });
    },
    async *chunk(statements, opts) {
      const explicit = opts?.maxStatements !== void 0 && opts.maxRequestBytes !== void 0;
      const caps = explicit ? null : await sessionLimits();
      const maxStatements = opts?.maxStatements ?? caps?.maxStatements ?? 0;
      const maxRequestBytes = opts?.maxRequestBytes ?? tighterCap(caps?.maxRequestBytes ?? 0, options.maxRequestBytes ?? 0);
      if (!explicit && maxStatements <= 0 && maxRequestBytes <= 0) {
        throw PluginError.fromWire("unsupported", "chunk needs the session limits (GuestDatabase.limits) or explicit maxStatements / maxRequestBytes");
      }
      const typed = statements.map((stmt) => stmt._asTyped());
      for (const indexes of chunkIndexes(typed, maxStatements, maxRequestBytes)) {
        yield indexes.map((i) => statements[i]);
      }
    },
    async writeChunks(statements, opts) {
      const from = opts.from ?? 0;
      const fail = (next2, err) => new WriteChunksError(next2, err instanceof PluginError ? err : PluginError.fromWire("internal", String(err)));
      const keyBytes = new TextEncoder().encode(opts.key).byteLength;
      if (keyBytes === 0 || keyBytes > MAX_WRITE_KEY_BYTES) {
        throw fail(from, PluginError.fromWire("invalid_params", `writeChunks key must be 1 to ${String(MAX_WRITE_KEY_BYTES)} bytes`));
      }
      if (!Number.isInteger(from) || from < 0 || from > statements.length) {
        throw fail(from, PluginError.fromWire("invalid_params", `writeChunks from ${String(from)} is not within the ${String(statements.length)} statements`));
      }
      const caps = {};
      if (opts.maxStatements !== void 0) {
        caps.maxStatements = opts.maxStatements;
      }
      if (opts.maxRequestBytes !== void 0) {
        caps.maxRequestBytes = opts.maxRequestBytes;
      }
      const batches = [];
      try {
        for await (const b of binding.chunk(statements.slice(from), caps)) {
          batches.push(b);
        }
      } catch (err) {
        throw fail(from, err);
      }
      let next = from;
      for (const b of batches) {
        try {
          if (opts.stop?.(next) === true) {
            return { next, done: false };
          }
          await binding.batch(b, {
            retry: { operationId: await writeChunksOperationId(opts.key, next), requestHash: "" }
          });
        } catch (err) {
          throw fail(next, err);
        }
        next += b.length;
      }
      return { next, done: true };
    }
  };
  binding._iterateRows = iterateRows;
  return binding;
}
function tighterCap(a, b) {
  if (a > 0 && b > 0) {
    return Math.min(a, b);
  }
  return a > 0 ? a : b;
}
function checkReplyIdentity(request, operationId, statementCount) {
  if (request.operationId !== "" && operationId !== request.operationId) {
    throw PluginError.fromWire("invalid_params", `reply answers operation ${operationId}, not ${request.operationId}`);
  }
  if (statementCount !== request.statements.length) {
    throw PluginError.fromWire("invalid_params", `reply carries ${statementCount} results for ${request.statements.length} statements`);
  }
}
function replyFor(request, reply) {
  checkReplyIdentity(request, reply.operationId, reply.statements.length);
  for (let i = 0; i < reply.statements.length; i++) {
    const maxRows = request.statements[i]?.maxRows ?? 0;
    const rows = reply.statements[i]?.rows.length ?? 0;
    if (maxRows > 0 && rows > maxRows) {
      throw PluginError.fromWire("invalid_params", `reply carries ${rows} rows for statement ${i}, which asked for at most ${maxRows}`);
    }
  }
  return reply;
}
function rowMap(columns, row) {
  const out = {};
  for (let i = 0; i < columns.length; i++) {
    const value = row.values[i];
    if (value !== void 0) {
      out[columns[i].name] = value;
    }
  }
  return out;
}
function chunkIndexes(typed, maxStatements, maxRequestBytes) {
  if (typed.length === 0) {
    return [];
  }
  const base = encodeExecuteRequest({
    operationId: RETRY_ID_PLACEHOLDER,
    requestHash: RETRY_HASH_PLACEHOLDER,
    statements: [PROBE_STATEMENT],
    deadlineUnixMs: 0
  }).byteLength - PROBE_STATEMENT_BYTES;
  const sizes = typed.map((stmt) => encodeExecuteRequest({
    operationId: "",
    requestHash: "",
    statements: [PROBE_STATEMENT, stmt],
    deadlineUnixMs: 0
  }).byteLength - PROBE_REQUEST_BYTES);
  const fits = (run2) => maxRequestBytes <= 0 || encodeExecuteRequest({
    operationId: RETRY_ID_PLACEHOLDER,
    requestHash: RETRY_HASH_PLACEHOLDER,
    statements: run2.map((i) => typed[i]),
    deadlineUnixMs: 0
  }).byteLength <= maxRequestBytes;
  const out = [];
  const emit = (run2) => {
    if (run2.length <= 1 || fits(run2)) {
      out.push(run2);
      return;
    }
    const half = Math.ceil(run2.length / 2);
    emit(run2.slice(0, half));
    emit(run2.slice(half));
  };
  let run = [];
  let bytes = base;
  for (let i = 0; i < typed.length; i++) {
    const size = sizes[i];
    const overStatements = maxStatements > 0 && run.length >= maxStatements;
    const overBytes = maxRequestBytes > 0 && run.length > 0 && bytes + size > maxRequestBytes;
    if (overStatements || overBytes) {
      emit(run);
      run = [];
      bytes = base;
    }
    run.push(i);
    bytes += size;
  }
  if (run.length > 0) {
    emit(run);
  }
  return out;
}
var RETRY_ID_PLACEHOLDER = "00000000-0000-4000-8000-000000000000";
var RETRY_HASH_PLACEHOLDER = "0".repeat(64);
var PROBE_STATEMENT = {
  sql: "",
  parameters: [],
  kind: "select",
  maxRows: 0,
  resultSelection: "rows"
};
var PROBE_REQUEST_BYTES = encodeExecuteRequest({
  operationId: "",
  requestHash: "",
  statements: [PROBE_STATEMENT],
  deadlineUnixMs: 0
}).byteLength;
var PROBE_STATEMENT_BYTES = encodeExecuteRequest({
  operationId: "",
  requestHash: "",
  statements: [PROBE_STATEMENT, PROBE_STATEMENT],
  deadlineUnixMs: 0
}).byteLength - PROBE_REQUEST_BYTES;
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
    iterate(options) {
      const typed = [this.asAll()._asTyped()];
      const streamedRows = binding._iterateRows;
      if (streamedRows !== void 0) {
        return streamedRows(typed, options?.retry);
      }
      return (async function* inline() {
        const reply = await binding.execute(typed, options);
        const result = reply.statements[0];
        if (result) {
          for (const row of result.rows) {
            yield rowMap(result.columns, row);
          }
        }
      })();
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
  const bytes = new Uint8Array(encodeExecuteRequest(canonical));
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
function hintResult(value) {
  if (value != null && typeof value === "object" && "records" in value) {
    const answer = value;
    return {
      found: answer.hint != null,
      // The wire leaves `hint` unset when not found.
      hint: answer.hint ?? void 0,
      records: Array.isArray(answer.records) ? answer.records : []
    };
  }
  return { found: value != null, hint: value ?? void 0, records: [] };
}
var okHits = (value) => okValue({ hits: asList(value) });
var okRecords = (value) => {
  if (!Array.isArray(value)) {
    throw PluginError.fromWire("internal", "metadata provider must return a MetadataRecord[]");
  }
  return okValue({ records: value });
};
var okCoverImages = (value) => {
  if (!Array.isArray(value)) {
    throw PluginError.fromWire("internal", "cover fetcher must return a CoverImage[]");
  }
  return okValue({ images: value });
};
var okFileRecord = (value) => {
  if (value === null || typeof value !== "object" || !Array.isArray(value.present)) {
    throw PluginError.fromWire("internal", "metadata file dialect must return a MetadataFileRecord");
  }
  return okValue({ record: value });
};
var okFileBytes = (value) => {
  if (typeof value === "string") {
    return okValue({ bytes: new TextEncoder().encode(value) });
  }
  if (!(value instanceof Uint8Array)) {
    throw PluginError.fromWire("internal", "metadata file dialect must render a Uint8Array or string");
  }
  return okValue({ bytes: value });
};
var okAiModels = (value) => {
  const v = value ?? {};
  if (!Array.isArray(v.models)) {
    throw PluginError.fromWire("internal", "models must return an object with a models list");
  }
  return okValue({ models: v.models });
};
var okAiRun = (value) => {
  const out = (value ?? {}).output?.textEmbeddings;
  if (!out || !(out.vectors instanceof Uint8Array)) {
    throw PluginError.fromWire("internal", "run must return output.textEmbeddings with vectors as a Uint8Array");
  }
  return okValue({
    output: {
      textEmbeddings: {
        dimensions: Number(out.dimensions),
        vectors: out.vectors,
        revision: String(out.revision ?? ""),
        fallbacks: Array.isArray(out.fallbacks) ? out.fallbacks.map(Number) : []
      }
    }
  });
};
var okNotifySent = (value) => {
  const v = value ?? {};
  const providerId = v.providerId === void 0 || v.providerId === null ? "" : String(v.providerId);
  return okValue(providerId ? { providerId } : {});
};
var okNotifyValidated = (value) => {
  const v = value ?? {};
  if (typeof v.addressJson !== "string") {
    throw PluginError.fromWire("internal", "validateDestination must return an object with an addressJson string");
  }
  const label = typeof v.label === "string" ? v.label : "";
  return okValue(label ? { addressJson: v.addressJson, label } : { addressJson: v.addressJson });
};
var okLines = (value) => okValue({ lines: asList(value).map((l) => String(l)) });
var okHealth = (value) => {
  const v = value ?? {};
  return okValue({ ok: Boolean(v.ok), detail: String(v.detail ?? "") });
};
function queueResult(outcome) {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack" };
    case "retry":
      return {
        kind: "retry",
        value: {
          delaySeconds: Number(outcome.delaySeconds) || 0,
          reason: String(outcome.reason ?? "")
        }
      };
    default:
      return { kind: "retry", value: { delaySeconds: 0, reason: "unknown queue outcome" } };
  }
}
function eventResult(outcome) {
  switch (outcome.kind) {
    case "ack":
      return { kind: "ack", value: { dummy: void 0 } };
    case "retry":
      return {
        kind: "retry",
        value: {
          retryAtUnixMs: Number(outcome.retryAtUnixMs) || 0,
          reason: String(outcome.reason ?? "")
        }
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
        value: {
          message: String(record.message ?? ""),
          bytesCopied: Number(record.bytesCopied) || 0
        }
      };
    case "retryable":
      return {
        kind: "retryable",
        value: {
          message: String(record.message ?? ""),
          retryAfterUnixMs: Number(record.retryAfterUnixMs) || 0
        }
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
    limit: options?.limit || void 0,
    withSignatures: options?.withSignatures || void 0,
    modifiedSinceMs: options?.modifiedSinceMs || void 0
  };
}
function moveOptions(options) {
  return {
    noOverwrite: options?.noOverwrite || void 0,
    ifSourceMatch: options?.ifSourceMatch || void 0
  };
}
function moveManyRequest(request) {
  const target = request?.target ?? { kind: "pairs", value: [] };
  if (target.kind === "pairs" && target.value.length > MAX_BATCH_KEYS) {
    throw new PluginError("payload_too_large", `moveMany of ${target.value.length} pairs`);
  }
  return {
    target,
    cursor: request?.cursor || void 0,
    limit: request?.limit || void 0,
    options: moveOptions(request?.options)
  };
}
function batchKeys(keys) {
  const list = keys ?? [];
  if (list.length === 0) {
    throw new PluginError("invalid_params", "a batch call needs at least one key");
  }
  if (list.length > MAX_BATCH_KEYS) {
    throw new PluginError("payload_too_large", `batch of ${list.length} keys`);
  }
  return list.map(String);
}
function wireOutcomes(outcomes) {
  return outcomes.map((o) => ({
    key: String(o.key ?? ""),
    error: {
      code: o.error ? String(o.error.code ?? "") || "internal" : "",
      message: o.error ? String(o.error.message ?? "") : "",
      details: ""
    }
  }));
}
function sessionCall(method) {
  return (s, p, cx) => {
    if (!cx.target) {
      throw new InvokeError(400, "invalid_params", `AdapterDatabaseSession.${method} requires X-Bookclerk-Target`);
    }
    const params = p;
    const args = method === "execute" || method === "executeStreamed" ? [params.request] : method === "importIdentity" ? [params.rows] : method === "dropUserRelations" ? [params.names] : [];
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
    purchaseHint: withParams("storefront", "purchaseHint", ContentSourcePurchaseHintParamsCodec, ContentSourcePurchaseHintResultsCodec, (value) => okValue(hintResult(value))),
    purchaseHints: withParams("storefront", "purchaseHints", ContentSourcePurchaseHintsParamsCodec, ContentSourcePurchaseHintsResultsCodec, (value) => okValue({
      results: asList(value).map(hintResult)
    })),
    listDeals: withParams("storefront", "listDeals", ContentSourceListDealsParamsCodec, ContentSourceListDealsResultsCodec, okHits),
    health: withoutParams("storefront", "health", ContentSourceHealthParamsCodec, ContentSourceHealthResultsCodec, okHealth),
    diagnose: withoutParams("storefront", "diagnose", ContentSourceDiagnoseParamsCodec, ContentSourceDiagnoseResultsCodec, okLines),
    catalogDetail: withParams("storefront", "catalogDetail", ContentSourceCatalogDetailParamsCodec, ContentSourceCatalogDetailResultsCodec, (value) => okValue({ found: value != null, hit: value ?? void 0 })),
    revokeAccount: withParams("storefront", "revokeAccount", ContentSourceRevokeAccountParamsCodec, ContentSourceRevokeAccountResultsCodec, okEmpty),
    qualityOffers: withParams("storefront", "qualityOffers", ContentSourceQualityOffersParamsCodec, ContentSourceQualityOffersResultsCodec, okPass)
  },
  Destination: {
    head: spec({
      params: DestinationHeadParamsCodec,
      results: DestinationHeadResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "head", [String(p.key ?? "")]),
      ok: (value) => {
        const meta = value ?? null;
        return okValue({
          found: meta != null,
          meta: meta ?? void 0
        });
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
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "commit", [
        String(p.key ?? ""),
        String(p.commitToken ?? "")
      ]),
      ok: okPass
    }),
    abortStage: spec({
      params: DestinationAbortStageParamsCodec,
      results: DestinationAbortStageResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "abortStage", [
        String(p.key ?? ""),
        String(p.commitToken ?? "")
      ]),
      ok: okEmpty
    }),
    move: spec({
      params: DestinationMoveParamsCodec,
      results: DestinationMoveResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "move", [
        String(p.from ?? ""),
        String(p.to ?? ""),
        moveOptions(p.options)
      ]),
      ok: okPass
    }),
    moveMany: spec({
      params: DestinationMoveManyParamsCodec,
      results: DestinationMoveManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "moveMany", [moveManyRequest(p.request)]),
      ok: (value) => {
        const page = value ?? { results: [] };
        return okValue({
          results: wireOutcomes(asList(page.results)),
          nextCursor: page.nextCursor ?? "",
          atomic: page.atomic ?? false,
          objectsMoved: page.objectsMoved ?? 0
        });
      }
    }),
    deleteMany: spec({
      params: DestinationDeleteManyParamsCodec,
      results: DestinationDeleteManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "deleteMany", [batchKeys(p.keys)]),
      ok: (value) => okValue({ results: wireOutcomes(asList(value)) })
    }),
    headMany: spec({
      params: DestinationHeadManyParamsCodec,
      results: DestinationHeadManyResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "headMany", [batchKeys(p.keys)]),
      ok: (value) => okValue({
        objects: asList(value).map((meta) => ({
          found: meta != null,
          meta: meta ?? void 0
        }))
      })
    }),
    translateKeys: spec({
      params: DestinationTranslateKeysParamsCodec,
      results: DestinationTranslateKeysResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "translateKeys", [batchKeys(p.keys)]),
      ok: (value) => okValue({ keys: asList(value).map(String) })
    }),
    changes: spec({
      params: DestinationChangesParamsCodec,
      results: DestinationChangesResultsCodec,
      stub: "storage",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "changes", [
        {
          cursor: p.request?.cursor || void 0,
          limit: p.request?.limit || void 0
        }
      ]),
      ok: okPass
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
  QueueConsumer: {
    queue: spec({
      params: QueueConsumerQueueParamsCodec,
      results: QueueConsumerQueueResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkQueue(cx.host.bindContext(cx.ctx), {
        queue: String(p.batch?.queue ?? ""),
        messages: Array.isArray(p.batch?.messages) ? p.batch.messages : []
      }),
      ok: (value) => okValue({ results: asList(value).map(queueResult) })
    })
  },
  ScheduledHandler: {
    scheduled: spec({
      params: ScheduledHandlerScheduledParamsCodec,
      results: ScheduledHandlerScheduledResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkScheduled(cx.host.bindContext(cx.ctx), p.controller ?? {}),
      ok: okEmpty
    })
  },
  SocketHandler: {
    message: spec({
      params: SocketHandlerMessageParamsCodec,
      results: SocketHandlerMessageResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkSocketMessage(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: (value) => okValue({
        pollEvents: value?.pollEvents === true
      })
    }),
    closed: spec({
      params: SocketHandlerClosedParamsCodec,
      results: SocketHandlerClosedResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkSocketClosed(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: okEmpty
    })
  },
  WebhookHandler: {
    delivered: spec({
      params: WebhookHandlerDeliveredParamsCodec,
      results: WebhookHandlerDeliveredResultsCodec,
      stub: "author",
      call: (s, p, cx) => s.bookclerkWebhookDelivered(cx.host.bindContext(cx.ctx), p.event ?? {}),
      ok: okEmpty
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
  MetadataProvider: {
    health: withoutParams("metadata", "health", MetadataProviderHealthParamsCodec, MetadataProviderHealthResultsCodec, okHealth),
    lookup: withParams("metadata", "lookup", MetadataProviderLookupParamsCodec, MetadataProviderLookupResultsCodec, okRecords),
    search: withParams("metadata", "search", MetadataProviderSearchParamsCodec, MetadataProviderSearchResultsCodec, okRecords),
    chapters: withParams("metadata", "chapters", MetadataProviderChaptersParamsCodec, MetadataProviderChaptersResultsCodec, okPass)
  },
  CoverFetcher: {
    health: withoutParams("covers", "health", CoverFetcherHealthParamsCodec, CoverFetcherHealthResultsCodec, okHealth),
    fetchCovers: spec({
      params: CoverFetcherFetchCoversParamsCodec,
      results: CoverFetcherFetchCoversResultsCodec,
      stub: "covers",
      call: (s, p, cx) => {
        if (!p.sink) {
          throw PluginError.fromWire("invalid_params", "fetchCovers needs a sink");
        }
        return named(s, cx.host.bindContext(cx.ctx), "fetchCovers", [p.params, p.sink]);
      },
      ok: okCoverImages
    })
  },
  AiModels: {
    health: withoutParams("ai", "health", AiModelsHealthParamsCodec, AiModelsHealthResultsCodec, okHealth),
    models: withoutParams("ai", "models", AiModelsModelsParamsCodec, AiModelsModelsResultsCodec, okAiModels),
    run: withParams("ai", "run", AiModelsRunParamsCodec, AiModelsRunResultsCodec, okAiRun)
  },
  Notifier: {
    health: withoutParams("notifier", "health", NotifierHealthParamsCodec, NotifierHealthResultsCodec, okHealth),
    send: withParams("notifier", "send", NotifierSendParamsCodec, NotifierSendResultsCodec, okNotifySent),
    validateDestination: withParams("notifier", "validateDestination", NotifierValidateDestinationParamsCodec, NotifierValidateDestinationResultsCodec, okNotifyValidated)
  },
  MetadataFiles: {
    health: withoutParams("metadataFiles", "health", MetadataFilesHealthParamsCodec, MetadataFilesHealthResultsCodec, okHealth),
    parse: withParams("metadataFiles", "parse", MetadataFilesParseParamsCodec, MetadataFilesParseResultsCodec, okFileRecord),
    render: withParams("metadataFiles", "render", MetadataFilesRenderParamsCodec, MetadataFilesRenderResultsCodec, okFileBytes)
  },
  Ingress: {
    start: withoutParams("ingress", "start", IngressStartParamsCodec, IngressStartResultsCodec, okPass),
    health: withoutParams("ingress", "health", IngressHealthParamsCodec, IngressHealthResultsCodec, okPass),
    stop: withoutParams("ingress", "stop", IngressStopParamsCodec, IngressStopResultsCodec, okEmpty)
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
    }),
    dropUnit: spec({
      params: DatabaseDropUnitParamsCodec,
      results: DatabaseDropUnitResultsCodec,
      stub: "databaseAdapter",
      call: (s, p, cx) => named(s, cx.host.bindContext(cx.ctx), "dropUnit", [String(p.unitRef ?? "")]),
      ok: okEmpty
    })
  },
  AdapterDatabaseSession: {
    // An isolate adapter's streamed frames travel inside one invoke reply,
    // so a streamed budget above one scalar could not be delivered; the
    // advertisement is clamped so the host's budget is one the adapter can
    // honour before it commits.
    capabilities: session("capabilities", AdapterDatabaseSessionCapabilitiesParamsCodec, AdapterDatabaseSessionCapabilitiesResultsCodec, (value) => okValue(clampIsolateStreamedCaps(value))),
    execute: session("execute", AdapterDatabaseSessionExecuteParamsCodec, AdapterDatabaseSessionExecuteResultsCodec, (value) => okValue(value)),
    close: session("close", AdapterDatabaseSessionCloseParamsCodec, AdapterDatabaseSessionCloseResultsCodec, okEmpty),
    bootstrap: session("bootstrap", AdapterDatabaseSessionBootstrapParamsCodec, AdapterDatabaseSessionBootstrapResultsCodec, (value) => okValue(value)),
    exportIdentity: session("exportIdentity", AdapterDatabaseSessionExportIdentityParamsCodec, AdapterDatabaseSessionExportIdentityResultsCodec, (value) => okValue(asList(value))),
    importIdentity: session("importIdentity", AdapterDatabaseSessionImportIdentityParamsCodec, AdapterDatabaseSessionImportIdentityResultsCodec, okEmpty),
    listUserRelations: session("listUserRelations", AdapterDatabaseSessionListUserRelationsParamsCodec, AdapterDatabaseSessionListUserRelationsResultsCodec, (value) => okValue(asList(value).map((n) => String(n)))),
    prepareUnitRestore: session("prepareUnitRestore", AdapterDatabaseSessionPrepareUnitRestoreParamsCodec, AdapterDatabaseSessionPrepareUnitRestoreResultsCodec, okEmpty),
    dropUserRelations: session("dropUserRelations", AdapterDatabaseSessionDropUserRelationsParamsCodec, AdapterDatabaseSessionDropUserRelationsResultsCodec, okEmpty),
    assertRestoreConstraints: session("assertRestoreConstraints", AdapterDatabaseSessionAssertRestoreConstraintsParamsCodec, AdapterDatabaseSessionAssertRestoreConstraintsResultsCodec, okEmpty),
    // An isolate adapter answers with the frames in memory, which travel as
    // the `inline` member of the invoke reply; frames past one scalar cannot
    // cross that reply, so they become a typed limit error instead of a
    // transport failure.
    executeStreamed: session("executeStreamed", AdapterDatabaseSessionExecuteStreamedParamsCodec, AdapterDatabaseSessionExecuteStreamedResultsCodec, (value) => inlineStreamReply(value))
  }
};
var CAP_KINDS = /* @__PURE__ */ new Set([
  "source",
  "destination",
  "progress",
  "cancellation"
]);
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
function clampIsolateStreamedCaps(caps) {
  const bytes = caps.maxStreamedResultBytes ?? 0;
  if (bytes <= MAX_SCALAR_BYTES) {
    return caps;
  }
  return { ...caps, maxStreamedResultBytes: MAX_SCALAR_BYTES };
}
function inlineStreamReply(frames) {
  if (!(frames instanceof Uint8Array)) {
    throw new InvokeError(500, "internal", "executeStreamed must return a Uint8Array of frames");
  }
  if (frames.byteLength > MAX_SCALAR_BYTES) {
    return {
      kind: "err",
      value: wireError(PluginError.limitExceeded({
        limit: "maxScalarBytes",
        cap: MAX_SCALAR_BYTES,
        used: frames.byteLength
      }))
    };
  }
  return { kind: "inline", value: frames };
}
function wireError(err) {
  const e = err;
  const code = e && typeof e === "object" ? typeof e.wireCode === "string" ? e.wireCode : typeof e.code === "string" ? e.code : "internal" : "internal";
  const message = err instanceof Error ? err.message : typeof err === "string" ? err : String(err);
  return { code, message, details: wireDetails(err) };
}
function wireDetails(err) {
  if (err instanceof PluginError)
    return err.detailsText;
  const details = err?.details;
  if (details === null || typeof details !== "object" || Array.isArray(details))
    return "";
  try {
    return JSON.stringify(details) ?? "";
  } catch {
    return "";
  }
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
    if ("value" in op && typeof op.value === "string") {
      return op.value;
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

// dist/sockets.js
function toBase64(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++)
    bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}
function fromBase64(text) {
  if (typeof text !== "string" || text === "")
    return new Uint8Array();
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++)
    out[i] = bin.charCodeAt(i);
  return out;
}
function sendBytes(data) {
  if (typeof data === "string")
    return null;
  if (data instanceof ArrayBuffer)
    return new Uint8Array(data);
  if (ArrayBuffer.isView(data))
    return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  throw PluginError.fromWire("invalid_params", "a socket message must be a string or bytes");
}
function routeFrame(data) {
  const bytes = sendBytes(data);
  const size = bytes ? bytes.byteLength : new TextEncoder().encode(data).byteLength;
  if (size > MAX_SOCKET_FRAME_BYTES) {
    throw PluginError.fromWire("payload_too_large", `socket message of ${size} bytes exceeds ${MAX_SOCKET_FRAME_BYTES}`);
  }
  return bytes ? { binary: toBase64(bytes) } : { text: data };
}
function socketMessageData(frame2) {
  if (frame2 && frame2.kind === "binary") {
    const bytes = frame2.value instanceof Uint8Array ? frame2.value : new Uint8Array();
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }
  return frame2 && typeof frame2.value === "string" ? frame2.value : "";
}
function encodeAttachment(value) {
  if (value === null || value === void 0)
    return new Uint8Array();
  let text;
  try {
    text = JSON.stringify(value);
  } catch (err) {
    throw PluginError.fromWire("invalid_params", `socket attachment is not JSON: ${String(err)}`);
  }
  if (text === void 0) {
    throw PluginError.fromWire("invalid_params", "socket attachment is not JSON");
  }
  const bytes = new TextEncoder().encode(text);
  if (bytes.byteLength > MAX_SOCKET_ATTACHMENT_BYTES) {
    throw PluginError.fromWire("payload_too_large", `socket attachment of ${bytes.byteLength} bytes exceeds ${MAX_SOCKET_ATTACHMENT_BYTES}`);
  }
  return bytes;
}
function decodeAttachment(bytes) {
  if (bytes.byteLength === 0)
    return null;
  const text = new TextDecoder().decode(bytes);
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}
async function call(transport, op, body) {
  if (!transport) {
    throw PluginError.fromWire("unavailable", "SOCKETS is not granted to this invocation");
  }
  const reply = await transport.call(op, body);
  if ("err" in reply) {
    throw PluginError.fromWire(reply.err.code || "internal", reply.err.message || "");
  }
  return reply.ok ?? {};
}
var HostSocket = class {
  id;
  url;
  protocol;
  #transport;
  #attachment;
  /**
   * @param transport - The adapter's grant.
   * @param fields - Socket id, URL, subprotocol and attachment.
   */
  constructor(transport, fields) {
    this.id = String(fields.socket ?? "");
    this.url = typeof fields.url === "string" ? fields.url : "";
    this.protocol = typeof fields.protocol === "string" ? fields.protocol : "";
    this.#transport = transport;
    this.#attachment = fields.attachment instanceof Uint8Array ? fields.attachment : fromBase64(fields.attachment);
  }
  async send(message) {
    await call(this.#transport, "send", { socket: this.id, frame: routeFrame(message) });
  }
  async close(code, reason) {
    await call(this.#transport, "close", {
      socket: this.id,
      code: Math.max(0, Math.floor(Number(code ?? 0) || 0)),
      reason: String(reason ?? "")
    });
  }
  async serializeAttachment(value) {
    const bytes = encodeAttachment(value);
    await call(this.#transport, "attachment", { socket: this.id, attachment: toBase64(bytes) });
    this.#attachment = bytes;
  }
  deserializeAttachment() {
    return decodeAttachment(this.#attachment);
  }
};
function hostWebSocketOf(transport, fields) {
  return new HostSocket(transport, fields);
}
function connectBody(url, options) {
  const headers = options?.headers;
  const pairs = Array.isArray(headers) ? headers : Object.entries(headers ?? {});
  return {
    url: String(url ?? ""),
    protocols: (options?.protocols ?? []).map(String),
    headers: pairs.map(([name, value]) => ({ name: String(name), value: String(value) })),
    autoResponses: (options?.autoResponses ?? []).map((pair) => ({
      request: routeFrame(pair.request),
      response: routeFrame(pair.response)
    })),
    attachment: toBase64(encodeAttachment(options?.attachment))
  };
}
function socketsBinding(transport) {
  return Object.freeze({
    async connect(url, options) {
      const info = await call(transport, "connect", connectBody(url, options));
      return new HostSocket(transport, info);
    },
    async getWebSockets() {
      const reply = await call(transport, "list", {});
      const sockets = Array.isArray(reply.sockets) ? reply.sockets : [];
      return sockets.map((info) => new HostSocket(transport, info));
    }
  });
}

// dist/origin.js
var ORIGIN_FRAME_PREFIX_BYTES = 4;
var MAX_ORIGIN_FRAME_HEAD_BYTES = 2 * (MAX_ORIGIN_HEADER_BYTES + MAX_ORIGIN_TARGET_BYTES) + 16 * 1024;
var HOP_BY_HOP = /* @__PURE__ */ new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade"
]);
var IGNORED = /* @__PURE__ */ new Set([
  "host",
  "forwarded",
  "x-forwarded-for",
  "x-forwarded-host",
  "x-forwarded-proto",
  "x-real-ip"
]);
function originTarget(url) {
  const parsed = new URL(url);
  return `${parsed.pathname}${parsed.search}`;
}
function requestHeaders(headers) {
  const out = [];
  headers.forEach((value, name) => {
    const lower = name.toLowerCase();
    if (HOP_BY_HOP.has(lower) || IGNORED.has(lower))
      return;
    out.push({ name: lower, value });
  });
  return out;
}
function bytesOf(text) {
  if (typeof text !== "string" || !text)
    return new Uint8Array();
  const binary = atob(text);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++)
    out[i] = binary.charCodeAt(i);
  return out;
}
function base64Of(bytes) {
  let binary = "";
  for (const byte of bytes)
    binary += String.fromCharCode(byte);
  return btoa(binary);
}
function connectResultOf(answer, sockets) {
  const socket = answer.socket;
  if (socket && typeof socket === "object") {
    return { socket: hostWebSocketOf(sockets, socket) };
  }
  const refused = answer.refused;
  if (refused && typeof refused === "object" && typeof refused.status === "number") {
    const headers = new Headers();
    if (Array.isArray(refused.headers)) {
      for (const field of refused.headers) {
        if (typeof field?.name === "string" && typeof field.value === "string") {
          headers.append(field.name, field.value);
        }
      }
    }
    const nullBody = [204, 205, 304].includes(refused.status);
    return {
      response: new Response(nullBody ? null : bytesOf(refused.body), {
        status: refused.status,
        headers
      })
    };
  }
  throw PluginError.fromWire("internal", "the origin's upgrade answer is neither a socket nor a refusal");
}
function originBinding(transport, sockets) {
  return Object.freeze({
    async connect(request, options) {
      const head = {
        method: request.method,
        target: originTarget(request.url),
        headers: requestHeaders(request.headers),
        ...options?.clientAddress ? { clientAddress: options.clientAddress } : {}
      };
      const attachment = base64Of(encodeAttachment(options?.attachment));
      const reply = await transport.connect(head, attachment);
      if ("err" in reply)
        throw PluginError.fromWire(reply.err.code, reply.err.message);
      return connectResultOf(reply.ok, sockets);
    },
    async fetch(request, options) {
      const head = {
        method: request.method,
        target: originTarget(request.url),
        headers: requestHeaders(request.headers),
        ...options?.clientAddress ? { clientAddress: options.clientAddress } : {}
      };
      const reply = await transport.fetch(head, request.body);
      if ("err" in reply)
        throw PluginError.fromWire(reply.err.code, reply.err.message);
      const headers = new Headers();
      for (const field of reply.ok.headers)
        headers.append(field.name, field.value);
      const nullBody = [101, 204, 205, 304].includes(reply.ok.status);
      return new Response(nullBody ? null : reply.ok.body, { status: reply.ok.status, headers });
    }
  });
}
function encodeOriginFrame(head) {
  const json = new TextEncoder().encode(JSON.stringify(head));
  if (json.length > MAX_ORIGIN_FRAME_HEAD_BYTES) {
    throw PluginError.fromWire("invalid_params", "origin request head is too large");
  }
  const out = new Uint8Array(ORIGIN_FRAME_PREFIX_BYTES + json.length);
  new DataView(out.buffer).setUint32(0, json.length, false);
  out.set(json, ORIGIN_FRAME_PREFIX_BYTES);
  return out;
}
function framedBody(frame2, body) {
  const reader = body?.getReader();
  let sentHead = false;
  return new ReadableStream({
    async pull(controller) {
      if (!sentHead) {
        sentHead = true;
        controller.enqueue(frame2);
        return;
      }
      if (!reader) {
        controller.close();
        return;
      }
      const { done, value } = await reader.read();
      if (done)
        controller.close();
      else
        controller.enqueue(value);
    },
    async cancel(reason) {
      await reader?.cancel(reason);
    }
  });
}
async function readOriginResponse(body) {
  const reader = body.getReader();
  let buffered = new Uint8Array(0);
  let need = -1;
  for (; ; ) {
    if (need < 0 && buffered.length >= ORIGIN_FRAME_PREFIX_BYTES) {
      need = new DataView(buffered.buffer, buffered.byteOffset, buffered.length).getUint32(0, false);
      if (need > MAX_ORIGIN_FRAME_HEAD_BYTES) {
        await reader.cancel();
        throw PluginError.fromWire("internal", "origin response head is too large");
      }
    }
    if (need >= 0 && buffered.length >= ORIGIN_FRAME_PREFIX_BYTES + need)
      break;
    const { done, value } = await reader.read();
    if (done)
      throw PluginError.fromWire("internal", "origin response ended inside its head");
    const next = new Uint8Array(buffered.length + value.length);
    next.set(buffered);
    next.set(value, buffered.length);
    buffered = next;
  }
  const end = ORIGIN_FRAME_PREFIX_BYTES + need;
  const head = parseResponseHead(new TextDecoder().decode(buffered.subarray(4, end)));
  const rest = buffered.slice(end);
  const out = new ReadableStream({
    start(controller) {
      if (rest.length > 0)
        controller.enqueue(rest);
    },
    async pull(controller) {
      const { done, value } = await reader.read();
      if (done)
        controller.close();
      else
        controller.enqueue(value);
    },
    async cancel(reason) {
      await reader.cancel(reason);
    }
  });
  return { ...head, body: out };
}
function parseResponseHead(text) {
  let value;
  try {
    value = JSON.parse(text);
  } catch {
    throw PluginError.fromWire("internal", "origin response head is not JSON");
  }
  const record = value ?? {};
  const status = record.status;
  if (typeof status !== "number" || !Number.isInteger(status) || status < 100 || status > 599) {
    throw PluginError.fromWire("internal", "origin response head has no valid status");
  }
  const headers = [];
  for (const field of Array.isArray(record.headers) ? record.headers : []) {
    const f = field ?? {};
    if (typeof f.name !== "string" || typeof f.value !== "string") {
      throw PluginError.fromWire("internal", "origin response head has a malformed header");
    }
    headers.push({ name: f.name, value: f.value });
  }
  return { status, headers };
}
async function grantedOriginFetch(granted, auth, head, body) {
  let frame2;
  try {
    frame2 = encodeOriginFrame(head);
  } catch (err) {
    return { err: errorOf(err) };
  }
  let resp;
  try {
    resp = await granted.fetch("http://granted/origin/fetch", {
      method: "POST",
      headers: { ...auth, "content-type": "application/octet-stream" },
      body: framedBody(frame2, body),
      // Node's fetch needs this for a streamed body; workerd ignores it.
      duplex: "half"
    });
  } catch (err) {
    return { err: { code: "unavailable", message: `origin fetch: ${messageOf(err)}` } };
  }
  if (resp.status !== 200 || !resp.body) {
    const value = await resp.json().catch(() => null);
    return {
      err: {
        code: value?.error?.code || "internal",
        message: value?.error?.message || `origin fetch HTTP ${resp.status}`
      }
    };
  }
  try {
    return { ok: await readOriginResponse(resp.body) };
  } catch (err) {
    return { err: errorOf(err) };
  }
}
async function grantedOriginConnect(granted, auth, head, attachment) {
  let resp;
  try {
    resp = await granted.fetch("http://granted/origin/connect", {
      method: "POST",
      headers: { ...auth, "content-type": "application/json" },
      body: JSON.stringify({ request: head, attachment })
    });
  } catch (err) {
    return { err: { code: "unavailable", message: `origin connect: ${messageOf(err)}` } };
  }
  const value = await resp.json().catch(() => null);
  if (resp.status !== 200 || !value || typeof value !== "object") {
    const error = value?.error ?? null;
    return {
      err: {
        code: error?.code || "internal",
        message: error?.message || `origin connect HTTP ${resp.status}`
      }
    };
  }
  return { ok: value };
}
function errorOf(err) {
  if (err instanceof PluginError)
    return { code: err.code, message: err.message };
  return { code: "internal", message: messageOf(err) };
}
function messageOf(err) {
  return err instanceof Error ? err.message : String(err);
}

// dist/webhooks.js
var DEFAULT_WEBHOOK_PULL_BATCH = 10;
function checkWebhookName(name) {
  const text = String(name ?? "");
  if (text.length === 0 || text.length > MAX_WEBHOOK_NAME_BYTES || !/^[a-z0-9][a-z0-9-]*$/.test(text)) {
    throw PluginError.fromWire("invalid_params", "malformed webhook name");
  }
  return text;
}
function checkLeases(leases) {
  const list = Array.isArray(leases) ? leases.map(String) : [];
  if (list.length > MAX_WEBHOOK_PULL_BATCH) {
    throw PluginError.fromWire("invalid_params", `a webhook ack names at most ${MAX_WEBHOOK_PULL_BATCH} leases in each list`);
  }
  for (const lease of list) {
    if (lease.length === 0 || lease.length > MAX_WEBHOOK_LEASE_ID_BYTES || !/^[A-Za-z0-9_-]+$/.test(lease)) {
      throw PluginError.fromWire("invalid_params", "malformed webhook lease id");
    }
  }
  return list;
}
function fromBase642(text) {
  if (typeof text !== "string" || text === "")
    return new Uint8Array();
  const bin = atob(text);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++)
    out[i] = bin.charCodeAt(i);
  return out;
}
async function call2(transport, op, body) {
  const reply = await transport.call(op, body);
  if ("err" in reply) {
    throw PluginError.fromWire(reply.err.code || "internal", reply.err.message || "");
  }
  return reply.ok ?? {};
}
function messageOf2(wire) {
  const headers = {};
  if (Array.isArray(wire.headers)) {
    for (const header of wire.headers) {
      if (header && typeof header.name === "string") {
        headers[header.name] = String(header.value ?? "");
      }
    }
  }
  return Object.freeze({
    leaseId: String(wire.leaseId ?? ""),
    timestamp: new Date(Number(wire.receivedAtUnixMs ?? 0) || 0),
    attempts: Number(wire.attempt ?? 0) || 0,
    headers: Object.freeze(headers),
    body: fromBase642(wire.body)
  });
}
function webhooksBinding(transport) {
  return Object.freeze({
    async url(name) {
      const reply = await call2(transport, "url", { name: checkWebhookName(name) });
      return typeof reply.url === "string" && reply.url !== "" ? reply.url : null;
    },
    async pull(name, options) {
      const batchSize = Math.floor(Number(options?.batchSize ?? DEFAULT_WEBHOOK_PULL_BATCH));
      if (!(batchSize >= 1 && batchSize <= MAX_WEBHOOK_PULL_BATCH)) {
        throw PluginError.fromWire("invalid_params", `webhook batch size must be 1..=${MAX_WEBHOOK_PULL_BATCH}`);
      }
      const visibilityTimeoutMs = Math.floor(Number(options?.visibilityTimeoutMs ?? 0));
      if (!(visibilityTimeoutMs >= 0 && visibilityTimeoutMs <= MAX_WEBHOOK_VISIBILITY_MS)) {
        throw PluginError.fromWire("invalid_params", `webhook visibility timeout must be at most ${MAX_WEBHOOK_VISIBILITY_MS} ms`);
      }
      const reply = await call2(transport, "pull", {
        name: checkWebhookName(name),
        batchSize,
        visibilityTimeoutMs
      });
      const deliveries = Array.isArray(reply.deliveries) ? reply.deliveries : [];
      return { messages: deliveries.map(messageOf2), missed: reply.missed === true };
    },
    async ack(name, leases) {
      const acks = checkLeases(leases?.acks);
      const retries = checkLeases(leases?.retries);
      if (retries.some((lease) => acks.includes(lease))) {
        throw PluginError.fromWire("invalid_params", "a webhook lease is both acknowledged and retried");
      }
      await call2(transport, "ack", { name: checkWebhookName(name), acks, retries });
    }
  });
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
  /**
   * Move one object (declare `storage.move`). Repeating a finished move
   * resolves with `alreadyMoved`.
   *
   * @param _from - Source key.
   * @param _to - Target key.
   * @param _options - Overwrite and source preconditions.
   * @returns How the move was done.
   */
  move(_from, _to, _options) {
    return Promise.reject(unsupported("move"));
  }
  /**
   * Move a set of objects or a whole prefix, one page per call (declare
   * `storage.moveMany`). Safe to repeat.
   *
   * @param _request - Pairs or prefix, cursor, and options.
   * @returns Per-key outcomes and the continuation.
   */
  moveMany(_request) {
    return Promise.reject(unsupported("moveMany"));
  }
  /**
   * Remove up to `MAX_BATCH_KEYS` objects (declare `storage.deleteMany`).
   *
   * @param _keys - Object keys.
   * @returns One outcome per key, in order; a missing key succeeds.
   */
  deleteMany(_keys) {
    return Promise.reject(unsupported("deleteMany"));
  }
  /**
   * Probe up to `MAX_BATCH_KEYS` objects (declare `storage.headMany`).
   *
   * @param _keys - Object keys.
   * @returns One entry per key, in order; `null` when missing.
   */
  headMany(_keys) {
    return Promise.reject(unsupported("headMany"));
  }
  /**
   * Keys changed since a cursor (declare `storage.changes`).
   *
   * @param _request - Cursor and page size.
   * @returns One page of changes.
   */
  changes(_request) {
    return Promise.reject(unsupported("changes"));
  }
  /**
   * Translate up to `MAX_BATCH_KEYS` host keys into the keys this
   * destination stores them under (declare `storage.translateKeys`). A host
   * key is `/`-joined segments, none empty, `.`, `..` or holding `\` or a
   * control character; return a relative key with the same properties. The
   * same host key must always translate to the same key. Without the
   * feature the host stores each host key as written.
   *
   * @param _keys - Host keys.
   * @returns One translated key per host key, in order.
   */
  translateKeys(_keys) {
    return Promise.reject(unsupported("translateKeys"));
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
   * The same atomic batch as {@link AdapterDatabaseSession.execute}, answered
   * as concatenated `ExecuteStreamFrame` messages counted against the
   * advertised `maxStreamedResultBytes` / `maxStreamedResultRows`; optional.
   * Adapters that advertise `0` for both leave the default, which answers
   * `unsupported`. An isolate adapter's frames travel inside one invoke
   * reply, so its streamed budget is clamped to `MAX_SCALAR_BYTES` (256 KiB)
   * and larger frames are a `limit_exceeded` (`maxScalarBytes`) error.
   * `executeAdapterBatchStreamed` runs the batch, stores the replay receipt
   * and checks that bound before it commits.
   *
   * @param _request - Proven statements plus isolation and receipt.
   * @returns Every frame of the committed reply, concatenated.
   */
  executeStreamed(_request) {
    return Promise.reject(unsupported("executeStreamed"));
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
    if (context.notify)
      merged.NOTIFY = context.notify;
    if (context.ai)
      merged.AI = context.ai;
    if (Array.isArray(context.databases)) {
      for (const entry of context.databases) {
        if (!entry || typeof entry.name !== "string" || !entry.name)
          continue;
        if (entry.database) {
          merged[entry.name] = entry.database;
        } else if (entry.transport) {
          merged[entry.name] = transportDatabase(entry.transport);
        }
      }
    }
    if (Array.isArray(context.queues)) {
      for (const entry of context.queues) {
        if (!entry || typeof entry.name !== "string" || !entry.name || !entry.queue)
          continue;
        merged[entry.name] = entry.queue;
      }
    }
    if (Array.isArray(context.kv)) {
      for (const entry of context.kv) {
        if (!entry || typeof entry.name !== "string" || !entry.name || !entry.kv)
          continue;
        merged[entry.name] = entry.kv;
      }
    }
    const sockets = socketsTransportOf(context);
    if (sockets)
      merged[sockets.name] = socketsBinding(sockets.sockets);
    if (context.origin)
      merged.ORIGIN = originBinding(context.origin, sockets?.sockets);
    if (context.webhooks)
      merged.WEBHOOKS = webhooksBinding(context.webhooks);
    if (context.network && typeof context.network === "object") {
      merged.NETWORK = Object.freeze(normalizeNetworkGrant(context.network));
    }
  }
  return Object.freeze(merged);
}
function normalizeNetworkGrant(grant) {
  return {
    outbound: grant.outbound === true,
    domains: Array.isArray(grant.domains) ? grant.domains : [],
    tcp: Array.isArray(grant.tcp) ? grant.tcp : [],
    settings: Array.isArray(grant.settings) ? grant.settings : []
  };
}
function transportDatabase(entry) {
  const transport = entry;
  const execute = typeof transport.executeBytes === "function" ? async (request) => decodeExecuteResultReply(await transport.executeBytes(encodeExecuteRequest(request))) : (request) => transport.execute(request);
  const readLimits = typeof transport.limitsBytes === "function" ? async () => decodeDbCapabilitiesReply(await transport.limitsBytes()) : typeof transport.limits === "function" ? () => transport.limits() : void 0;
  const streamed = typeof transport.openStreamedBytes === "function" ? (request) => transport.openStreamedBytes(encodeExecuteRequest(request)) : typeof transport.executeStreamed === "function" ? (request) => transport.executeStreamed(request) : void 0;
  return createDatabaseBinding({
    execute,
    ...readLimits ? { limits: readLimits } : {},
    ...streamed ? { executeStreamed: streamed } : {}
  });
}
function socketsTransportOf(context) {
  const list = context && Array.isArray(context.sockets) ? context.sockets : [];
  return list.find((entry) => entry && typeof entry.name === "string" && entry.name !== "" && entry.sockets);
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
var QueueMessage = class {
  #result = null;
  /** Outbox row id; stable across redeliveries. */
  id;
  /** When the producer sent the message. */
  timestamp;
  /** Encoded body bytes. */
  body;
  /** Body encoding: `json`, `text`, or `bytes`. */
  contentType;
  /** Delivery counter, starting at 1. */
  attempts;
  /** Producer plugin id; empty when the host sent it. */
  source;
  /** Raw wire envelope. */
  raw;
  constructor(message) {
    const m = message && typeof message === "object" ? message : {};
    this.id = String(m.id ?? "");
    this.timestamp = new Date(Number(m.timestampUnixMs ?? 0) || 0);
    const body = m.body;
    this.body = body instanceof Uint8Array ? body : body instanceof ArrayBuffer ? new Uint8Array(body) : new Uint8Array();
    this.contentType = String(m.contentType ?? "") || "json";
    this.attempts = Number(m.attempts ?? 1) || 1;
    this.source = String(m.source ?? "");
    this.raw = m;
  }
  /**
   * Decode {@link QueueMessage.body} as JSON.
   *
   * @returns Parsed body (`null` for an empty body).
   */
  json() {
    if (this.body.byteLength === 0)
      return null;
    return JSON.parse(new TextDecoder().decode(this.body));
  }
  /**
   * Decode {@link QueueMessage.body} as UTF-8 text.
   *
   * @returns The body text.
   */
  text() {
    return new TextDecoder().decode(this.body);
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
  /** Mark handled; the host marks the message delivered. */
  ack() {
    this.#record({ kind: "ack" });
  }
  /**
   * Redeliver later: after `delaySeconds`, or — when omitted — after the
   * consumer's `retry_delay`.
   *
   * @param options - Redelivery timing and reason.
   */
  retry(options) {
    this.#record({
      kind: "retry",
      delaySeconds: Math.max(0, Math.floor(Number(options?.delaySeconds ?? 0) || 0)),
      reason: String(options?.reason ?? "")
    });
  }
};
var QueueBatch = class {
  /** Queue the messages were sent to. */
  queue;
  /** Messages in delivery order. */
  messages;
  /** Invocation identity of this delivery. */
  invocation;
  constructor(queue, messages, invocation) {
    this.queue = String(queue ?? "");
    this.messages = Object.freeze((Array.isArray(messages) ? messages : []).map((message) => new QueueMessage(message)));
    this.invocation = invocation ?? invocationOf(void 0);
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
function queueBatchResults(batch, failed) {
  return batch.messages.map((m) => {
    if (m.result)
      return m.result;
    if (failed !== void 0) {
      return { kind: "retry", delaySeconds: 0, reason: errorMessage(failed) };
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
   * Adapter dispatch for the `queue(batch)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wireBatch - Wire `QueueBatch`.
   * @returns One outcome per message, in order.
   * @internal
   */
  async bookclerkQueue(context, wireBatch) {
    if (typeof this.queue !== "function") {
      throw unsupported("queue");
    }
    applyInvocationEnv(this, context);
    const messages = Array.isArray(wireBatch?.messages) ? wireBatch.messages : [];
    const batch = new QueueBatch(wireBatch?.queue, messages, invocationOf(context));
    try {
      await this.queue(batch);
      return queueBatchResults(batch, void 0);
    } catch (err) {
      return queueBatchResults(batch, err ?? new Error("queue handler failed"));
    }
  }
  /**
   * Adapter dispatch for the `scheduled(controller)` trigger.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `ScheduledController`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkScheduled(context, wire) {
    if (typeof this.scheduled !== "function") {
      throw unsupported("scheduled");
    }
    applyInvocationEnv(this, context);
    await this.scheduled(scheduledControllerOf(wire));
  }
  /**
   * Adapter dispatch for `SocketHandler.message`.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `SocketMessage`.
   * @returns What the handler asks of the host (`pollEvents`).
   * @internal
   */
  async bookclerkSocketMessage(context, wire) {
    if (typeof this.webSocketMessage !== "function") {
      throw unsupported("webSocketMessage");
    }
    applyInvocationEnv(this, context);
    const ws = hostWebSocketOf(socketsTransportOf(context)?.sockets, {
      socket: wire?.socket,
      attachment: wire?.attachment
    });
    const asked = await this.webSocketMessage(ws, socketMessageData(wire?.frame));
    return { pollEvents: asked?.pollEvents === true };
  }
  /**
   * Adapter dispatch for `SocketHandler.closed`. Without a
   * `webSocketClose` handler the close is acknowledged and ignored.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `SocketClosed`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkSocketClosed(context, wire) {
    if (typeof this.webSocketClose !== "function")
      return;
    applyInvocationEnv(this, context);
    const ws = hostWebSocketOf(socketsTransportOf(context)?.sockets, {
      socket: wire?.socket,
      attachment: wire?.attachment
    });
    await this.webSocketClose(ws, Number(wire?.code ?? 0) || 0, String(wire?.reason ?? ""), wire?.clean === true);
  }
  /**
   * Adapter dispatch for `WebhookHandler.delivered`. Without a
   * `webhookDelivered` handler the call is acknowledged and ignored.
   *
   * @param context - Granted bindings for this invocation.
   * @param wire - Wire `WebhookDelivered`.
   * @returns Resolves when the author's handler completes.
   * @internal
   */
  async bookclerkWebhookDelivered(context, wire) {
    if (typeof this.webhookDelivered !== "function")
      return;
    applyInvocationEnv(this, context);
    await this.webhookDelivered(Object.freeze({
      name: String(wire?.name ?? ""),
      waiting: Number(wire?.waiting ?? 0) || 0
    }));
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
var PURCHASE_HINTS_BUDGET_MS = 4e3;
var PURCHASE_HINTS_CONCURRENCY = 4;
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
  "purchaseHints",
  "listDeals",
  "catalogDetail",
  "revokeAccount",
  "qualityOffers",
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
   * One page of one account's library scan.
   *
   * The host calls this repeatedly, passing back `nextCursor` as
   * `params.cursor` until a page returns an empty `nextCursor`, which ends
   * the pass. An empty `params.syncToken` asks for a full pass; a non-empty
   * one asks only for changes since that token (list gone titles in
   * `removed`). Set `complete` only on the last page of a full pass that
   * listed the whole library. Only the last page's `syncToken` is stored. Reject a stale cursor or token with `invalid_cursor` and the
   * host restarts as a full pass.
   *
   * @param _params - The account, its credential, the cursor and the sync token.
   * @returns At most `MAX_SCAN_PAGE_BOOKS` books plus the paging fields.
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
   * `params.lookup` says how the host names the title: `"isbn"` or
   * `"asin"` (sent only when `describe().catalog` declares `priceByIsbn` /
   * `priceByAsin`) want that exact edition or `null`, never a title match;
   * `"product"` names the store's own product; `"title"` asks for a search
   * by title and authors, answered best with a {@link PurchaseHintAnswer}
   * listing every priced result; `"unspecified"` (older hosts) leaves the
   * choice to the store.
   *
   * @param _params - Title identifiers and how to look the title up.
   * @returns Hint, an answer with title-search records, or `null`.
   */
  purchaseHint(_params) {
    return Promise.reject(unsupported("purchaseHint"));
  }
  /**
   * Purchase links / price hints for several titles in one call, one
   * answer per query in order. The default asks {@link purchaseHint} for
   * each query, {@link PURCHASE_HINTS_CONCURRENCY} at a time, those with a
   * product id first, and stops starting lookups after
   * {@link PURCHASE_HINTS_BUDGET_MS}; a store whose catalog prices
   * many products per request overrides this to make one request. Reject with a
   * `rate_limited` {@link PluginError} (its `details.retryAfterMs` says how
   * long) when the store asks to slow down, so the host leaves it alone.
   *
   * @param params - The titles to price, in order.
   * @returns One hint, answer with records, or `null` per query.
   */
  async purchaseHints(params) {
    const started = Date.now();
    const out = params.queries.map(() => null);
    const order = params.queries.map((query, i) => ({ i, direct: Boolean(query.productId?.trim()) })).sort((a, b) => Number(b.direct) - Number(a.direct)).map((entry) => entry.i);
    let next = 0;
    let failed = false;
    const lane = async () => {
      while (next < order.length && !failed) {
        if (Date.now() - started >= PURCHASE_HINTS_BUDGET_MS)
          return;
        const i = order[next++];
        try {
          out[i] = await this.purchaseHint(params.queries[i]);
        } catch (err) {
          failed = true;
          throw err;
        }
      }
    };
    const lanes = Math.min(PURCHASE_HINTS_CONCURRENCY, order.length);
    await Promise.all(Array.from({ length: lanes }, lane));
    return out;
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
   * Undo store-side state for one account before the host deletes its
   * credentials, such as deregistering the device registered at login.
   * Best effort: the host revokes the account whatever this returns. The
   * default does nothing.
   *
   * @param _params - Account id and its stored credentials.
   * @returns Resolves when the store sign-out finished.
   */
  revokeAccount(_params) {
    return Promise.resolve();
  }
  /**
   * The audio quality one account would download for each title today, one
   * answer per title id in order (`found: false` when the store cannot
   * say). It must not license or download anything. The host asks only a
   * store that declares `catalog.qualityOffers` in `describe()`; the
   * default rejects as unsupported.
   *
   * @param _params - Account, its credentials, settings and title ids.
   * @returns One answer per title, plus any refreshed credentials.
   */
  qualityOffers(_params) {
    return Promise.reject(unsupported("qualityOffers"));
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
  "abortStage",
  "move",
  "moveMany",
  "deleteMany",
  "headMany",
  "changes",
  "translateKeys"
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
  /**
   * Move one object (declare `storage.move`). Repeating a finished move
   * resolves with `alreadyMoved`.
   *
   * @param _from - Source key.
   * @param _to - Target key.
   * @param _options - Overwrite and source preconditions.
   * @returns How the move was done.
   */
  move(_from, _to, _options) {
    return Promise.reject(unsupported("move"));
  }
  /**
   * Move a set of objects or a whole prefix, one page per call (declare
   * `storage.moveMany`). Safe to repeat.
   *
   * @param _request - Pairs or prefix, cursor, and options.
   * @returns Per-key outcomes and the continuation.
   */
  moveMany(_request) {
    return Promise.reject(unsupported("moveMany"));
  }
  /**
   * Remove up to `MAX_BATCH_KEYS` objects (declare `storage.deleteMany`).
   *
   * @param _keys - Object keys.
   * @returns One outcome per key, in order; a missing key succeeds.
   */
  deleteMany(_keys) {
    return Promise.reject(unsupported("deleteMany"));
  }
  /**
   * Probe up to `MAX_BATCH_KEYS` objects (declare `storage.headMany`).
   *
   * @param _keys - Object keys.
   * @returns One entry per key, in order; `null` when missing.
   */
  headMany(_keys) {
    return Promise.reject(unsupported("headMany"));
  }
  /**
   * Keys changed since a cursor (declare `storage.changes`).
   *
   * @param _request - Cursor and page size.
   * @returns One page of changes.
   */
  changes(_request) {
    return Promise.reject(unsupported("changes"));
  }
  /**
   * Translate up to `MAX_BATCH_KEYS` host keys into the keys this
   * destination stores them under (declare `storage.translateKeys`). A host
   * key is `/`-joined segments, none empty, `.`, `..` or holding `\` or a
   * control character; return a relative key with the same properties. The
   * same host key must always translate to the same key. Without the
   * feature the host stores each host key as written.
   *
   * @param _keys - Host keys.
   * @returns One translated key per host key, in order.
   */
  translateKeys(_keys) {
    return Promise.reject(unsupported("translateKeys"));
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
  "assertRestoreConstraints",
  "executeStreamed"
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
  static bookclerkMethods = Object.freeze(["openSession", "dropUnit"]);
  /**
   * Opens a session. Sessions cannot survive suspension.
   *
   * @returns Adapter session capability.
   */
  openSession() {
    return Promise.reject(unsupported("openSession"));
  }
  /**
   * Physically drops one provisioned binding unit.
   *
   * @param _unitRef - Adapter-native unit identity.
   * @returns Resolves when the unit is dropped.
   */
  dropUnit(_unitRef) {
    return Promise.reject(unsupported("dropUnit"));
  }
  /**
   * Adapter dispatch: `openSession` → session id; `dropUnit`; `session` → call
   * on a retained session (`args` = `[id, method, ...params]`).
   *
   * @param context - Granted bindings for this invocation.
   * @param method - `openSession`, `dropUnit`, or `session`.
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
    if (method === "dropUnit") {
      applyInvocationEnv(this, context);
      this.invocation = invocationOf(context);
      await this.dropUnit(String(args[0] ?? ""));
      return void 0;
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
var MetadataEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["health", "lookup", "search", "chapters"]);
  /**
   * Reports whether the provider can answer.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Records for the editions the identifiers name.
   *
   * @param _params - Identifiers, marketplace and result cap.
   * @returns Records in relevance order (`[]` when nothing matched).
   */
  lookup(_params) {
    return Promise.reject(unsupported("lookup"));
  }
  /**
   * Records matching a title and, optionally, an author.
   *
   * @param _params - Title, author, language, marketplace and result cap.
   * @returns Records in relevance order (`[]` when nothing matched).
   */
  search(_params) {
    return Promise.reject(unsupported("search"));
  }
  /**
   * Chapter layout of one edition.
   *
   * @param _params - Identifiers of the edition.
   * @returns The layout, or `{ found: false }` when the provider has none.
   */
  chapters(_params) {
    return Promise.reject(unsupported("chapters"));
  }
};
var CoverEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["health", "fetchCovers"]);
  /**
   * Reports whether the fetcher can answer.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Fetches each URL and streams its body into `sink` under its index.
   *
   * @param _params - URLs (absolute `https://`) and the per-body byte cap.
   * @param _sink - Host destination; accepts `put` for keys `0..urls.length`.
   * @returns One entry per URL, in request order.
   */
  fetchCovers(_params, _sink) {
    return Promise.reject(unsupported("fetchCovers"));
  }
};
var MetadataFilesEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["health", "parse", "render"]);
  /**
   * Reports whether the dialects can answer.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * What one file says about its edition. List every field the file states
   * in `present`, including ones it states as empty; leave out fields it
   * does not mention. Throw `invalid_params` for a file the dialect cannot
   * read.
   *
   * @param _params - Dialect id, file name and the whole file.
   * @returns The parsed record.
   */
  parse(_params) {
    return Promise.reject(unsupported("parse"));
  }
  /**
   * The new file: `params.existing` with each field in `params.fields` set to
   * the record's value (removed when the value is empty), everything else
   * kept as it was. `existing` is empty when the host creates the file.
   *
   * @param _params - Dialect id, values, fields to set and the current file.
   * @returns The new file's bytes; a string is written as UTF-8.
   */
  render(_params) {
    return Promise.reject(unsupported("render"));
  }
};
var AiEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["health", "models", "run"]);
  /**
   * Reports whether the models can be loaded with the current settings.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Lists the models served now. Must be cheap: do not load a model here.
   * Change a model's `revision` whenever its weights, tokenizer or loaded
   * language packs change, because outputs are comparable only within one
   * revision.
   *
   * @returns The served models, each `modelId` once.
   */
  models() {
    return Promise.resolve({ models: [] });
  }
  /**
   * Runs one model on one input. Throw `not_found` for a model this plugin
   * does not serve and `invalid_params` for an input of another task. For
   * text embeddings return one vector per text, in input order
   * ({@link packEmbeddingVectors}), with the model's listed `dimensions` and
   * `revision`.
   *
   * @param _params - The plugin's own model id and the input.
   * @returns The output; leave `model` unset.
   */
  run(_params) {
    return Promise.reject(unsupported("run"));
  }
};
var NotifierEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["health", "send", "validateDestination"]);
  /**
   * Reports whether the service can be reached with the current settings.
   *
   * @returns Health flag plus detail.
   */
  health() {
    return Promise.resolve({ ok: true, detail: "" });
  }
  /**
   * Delivers one message to one destination. Throw `rate_limited` (with
   * `details.retryAfterMs` when the service says when) or `unavailable` for
   * a failure worth retrying; any other error fails this destination. Never
   * log `params.message` when `sensitive` is set, nor `addressJson`.
   *
   * @param _params - The rendered message and its destination.
   * @returns The service's message id, when it gives one.
   */
  send(_params) {
    return Promise.reject(unsupported("send"));
  }
  /**
   * Checks and normalises a personal address a patron entered. Throw
   * `invalid_params` for an address the service would refuse. The default
   * accepts the address as given.
   *
   * @param params - The address document as entered.
   * @returns The address to save and an optional label that is never a secret.
   */
  validateDestination(params) {
    return Promise.resolve({ addressJson: params.addressJson });
  }
};
var IngressEntrypoint = class extends NamedEntrypoint {
  static bookclerkMethods = Object.freeze(["start", "health", "stop"]);
  /**
   * Brings the public address up and returns it once it serves.
   *
   * @returns The public origin: `https://` and a host name, no port, path
   *   or trailing slash.
   */
  start() {
    return Promise.reject(unsupported("start"));
  }
  /**
   * Reports whether the address is serving.
   *
   * @returns `serving`, plus a detail for the operator when it is not.
   */
  health() {
    return Promise.resolve({ serving: true });
  }
  /**
   * Takes the public address down. The host stops sending requests after
   * this returns.
   *
   * @returns Resolves once the address no longer serves.
   */
  stop() {
    return Promise.resolve();
  }
};
var ENTRYPOINT_BINDINGS = Object.freeze({
  storefront: "PLUGIN_STOREFRONT",
  storage: "PLUGIN_STORAGE",
  databaseAdapter: "PLUGIN_DATABASE_ADAPTER",
  remoteLibrary: "PLUGIN_REMOTE_LIBRARY",
  cli: "PLUGIN_CLI",
  oidc: "PLUGIN_OIDC",
  metadata: "PLUGIN_METADATA",
  covers: "PLUGIN_COVERS",
  metadataFiles: "PLUGIN_METADATA_FILES",
  notifier: "PLUGIN_NOTIFIER",
  ingress: "PLUGIN_INGRESS",
  ai: "PLUGIN_AI"
});
var ENTRYPOINT_CLASSES = Object.freeze({
  storefront: "Storefront",
  storage: "Storage",
  databaseAdapter: "DatabaseAdapter",
  remoteLibrary: "RemoteLibrary",
  cli: "Cli",
  oidc: "Oidc",
  metadata: "Metadata",
  covers: "Covers",
  metadataFiles: "MetadataFiles",
  notifier: "Notifier",
  ingress: "Ingress",
  ai: "Ai"
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
    if (options?.ifMatch || options?.ifNoneMatch) {
      throw PluginError.fromWire("unsupported", "conditional writes are not supported on granted destinations");
    }
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
function packEmbeddingVectors(vectors) {
  const width = vectors.length > 0 ? vectors[0].length : 0;
  const out = new Uint8Array(vectors.length * width * 4);
  const view = new DataView(out.buffer);
  vectors.forEach((vector, row) => {
    if (vector.length !== width) {
      throw PluginError.fromWire("internal", "embedding vectors differ in length");
    }
    for (let i = 0; i < width; i++) {
      view.setFloat32((row * width + i) * 4, vector[i], true);
    }
  });
  return out;
}
function unpackEmbeddingVectors(bytes, dimensions) {
  const width = dimensions * 4;
  if (!Number.isInteger(dimensions) || dimensions < 1 || bytes.length % width !== 0) {
    throw PluginError.fromWire("internal", "embedding bytes are not whole vectors");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const out = [];
  for (let at = 0; at < bytes.length; at += width) {
    const vector = new Array(dimensions);
    for (let i = 0; i < dimensions; i++)
      vector[i] = view.getFloat32(at + i * 4, true);
    out.push(vector);
  }
  return out;
}
var GrantedAi = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the ai grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Run one model on one input.
   *
   * @param model - Model name or task alias.
   * @param input - The input of the model's task.
   * @returns The model's output.
   */
  async run(model, input) {
    if (typeof model !== "string" || !model) {
      throw PluginError.fromWire("invalid_params", "run requires a model name");
    }
    const text = input?.text;
    const list = typeof text === "string" ? [text] : text;
    if (!Array.isArray(list) || list.some((t) => typeof t !== "string")) {
      throw PluginError.fromWire("invalid_params", "run requires input.text as a string or strings");
    }
    const role = input.role ?? "document";
    const wire = {
      model,
      input: { textEmbeddings: { texts: list.map((t) => ({ text: t, role })) } }
    };
    const resp = await this.#granted.fetch("http://granted/ai/run", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire)
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `ai run HTTP ${resp.status}`);
    }
    const out = value.output?.textEmbeddings ?? {};
    const dimensions = Number(out.dimensions ?? 0);
    const bytes = typeof out.vectors === "string" ? base64ToBytes(out.vectors) : new Uint8Array();
    const data = unpackEmbeddingVectors(bytes, dimensions);
    return {
      shape: [data.length, dimensions],
      data,
      model: String(value.model ?? model),
      revision: String(out.revision ?? ""),
      fallbacks: Array.isArray(out.fallbacks) ? out.fallbacks.map(Number) : []
    };
  }
};
function notifyFields(fields) {
  if (fields === void 0)
    return [];
  if (Array.isArray(fields)) {
    return fields.map((f) => ({ name: String(f?.name ?? ""), value: String(f?.value ?? "") }));
  }
  if (fields === null || typeof fields !== "object") {
    throw PluginError.fromWire("invalid_params", "fields must be a list or an object");
  }
  return Object.entries(fields).map(([name, value]) => ({ name, value: String(value) }));
}
var GrantedNotify = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the notify grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Raise one declared notification type.
   *
   * @param raise - Type, fields, optional patron, link path and alert.
   * @returns The host's notification id.
   */
  async raise(raise) {
    if (!raise || typeof raise.notificationType !== "string" || !raise.notificationType) {
      throw PluginError.fromWire("invalid_params", "raise requires notificationType");
    }
    const wire = {
      notificationType: raise.notificationType,
      fields: notifyFields(raise.fields),
      patronId: raise.patronId || void 0,
      linkPath: raise.linkPath || void 0,
      groupKey: raise.groupKey || void 0,
      alertAction: raise.alertAction ?? "none",
      alertKey: raise.alertKey || void 0
    };
    const resp = await this.#granted.fetch("http://granted/notify/raise", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire)
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `notify raise HTTP ${resp.status}`);
    }
    return { notificationId: String(value.notificationId ?? "") };
  }
};
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
  /**
   * Read one page of past events the granted subscriptions match.
   *
   * @param options - Start position, types, and page size.
   * @returns One page of events and the cursor after it.
   */
  async read(options = {}) {
    const since = options.since instanceof Date ? options.since.getTime() : options.since ?? options.sinceUnixMs;
    if (since !== void 0 && (!Number.isSafeInteger(since) || since < 0)) {
      throw PluginError.fromWire("invalid_params", "since must be a non-negative instant");
    }
    const limit = options.limit ?? 0;
    if (!Number.isSafeInteger(limit) || limit < 0) {
      throw PluginError.fromWire("invalid_params", "limit must be a non-negative integer");
    }
    const resp = await this.#granted.fetch("http://granted/events/read", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify({
        after: String(options.after ?? ""),
        sinceUnixMs: since ?? 0,
        types: (options.types ?? []).map(String),
        limit
      })
    });
    const value = await resp.json().catch((error) => {
      if (resp.ok) {
        throw PluginError.fromWire("internal", `events read reply is not JSON: ${String(error)}`);
      }
      return {};
    });
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `events read HTTP ${resp.status}`);
    }
    const events = Array.isArray(value.events) ? value.events : [];
    return {
      events: events.map((event) => fromBridgeJson(event)),
      cursor: String(value.cursor ?? ""),
      more: Boolean(value.more),
      expiredEventIds: Array.isArray(value.expiredEventIds) ? value.expiredEventIds.map(String) : [],
      truncated: Boolean(value.truncated)
    };
  }
};
function queueBodyBytes(body, contentType) {
  const kind = contentType || "json";
  if (kind === "bytes") {
    if (body instanceof Uint8Array)
      return { body, contentType: kind };
    if (body instanceof ArrayBuffer)
      return { body: new Uint8Array(body), contentType: kind };
    throw PluginError.fromWire("invalid_params", "bytes queue message requires a Uint8Array body");
  }
  if (kind === "text") {
    if (typeof body !== "string") {
      throw PluginError.fromWire("invalid_params", "text queue message requires a string body");
    }
    return { body: new TextEncoder().encode(body), contentType: kind };
  }
  if (kind !== "json") {
    throw PluginError.fromWire("invalid_params", `unknown queue content type \`${kind}\``);
  }
  return { body: new TextEncoder().encode(JSON.stringify(body ?? null)), contentType: kind };
}
var GrantedQueue = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the queue grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Send one message.
   *
   * @param body - JSON value, string, or bytes per `options.contentType`.
   * @param options - Encoding and delay.
   * @returns The outbox id the host assigned.
   */
  async send(body, options) {
    return this.sendBatch([{ body, ...options ?? {} }]);
  }
  /**
   * Send up to `maxListPage` messages in order.
   *
   * @param messages - Bodies with per-message options.
   * @param options - Default delay for entries that carry none.
   * @returns The outbox ids in send order.
   */
  async sendBatch(messages, options) {
    if (!Array.isArray(messages) || messages.length === 0) {
      throw PluginError.fromWire("invalid_params", "sendBatch requires at least one message");
    }
    if (messages.length > MAX_LIST_PAGE) {
      throw PluginError.fromWire("payload_too_large", `queue batch of ${messages.length} exceeds ${MAX_LIST_PAGE}`);
    }
    const wireMessages = messages.map((entry) => {
      const encoded = queueBodyBytes(entry?.body, entry?.contentType);
      if (encoded.body.byteLength > MAX_EVENT_PAYLOAD_BYTES) {
        throw PluginError.fromWire("payload_too_large", `queue message body of ${encoded.body.byteLength} bytes exceeds ${MAX_EVENT_PAYLOAD_BYTES}`);
      }
      return {
        body: encoded.body,
        contentType: encoded.contentType,
        delaySeconds: Math.max(0, Math.floor(Number(entry?.delaySeconds ?? 0) || 0))
      };
    });
    const wire = toBridgeJson({
      messages: wireMessages,
      delaySeconds: Math.max(0, Math.floor(Number(options?.delaySeconds ?? 0) || 0))
    });
    const resp = await this.#granted.fetch("http://granted/queues/send", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(wire)
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `queue send HTTP ${resp.status}`);
    }
    return {
      messageIds: Array.isArray(value.messageIds) ? value.messageIds.map((id) => String(id)) : []
    };
  }
};
function checkKvKey(key) {
  if (typeof key !== "string" || key.length === 0) {
    throw PluginError.fromWire("invalid_params", "kv key must be a non-empty string");
  }
  const bytes = new TextEncoder().encode(key).byteLength;
  if (bytes > MAX_KV_KEY_BYTES) {
    throw PluginError.fromWire("payload_too_large", `kv key of ${bytes} bytes exceeds ${MAX_KV_KEY_BYTES}`);
  }
  return key;
}
function parseKvMetadata(text) {
  if (typeof text !== "string" || text === "")
    return null;
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}
function decodeKvValue(bytes, type) {
  if (type === "arrayBuffer") {
    return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
  }
  const text = new TextDecoder().decode(bytes);
  if (type === "json")
    return JSON.parse(text);
  return text;
}
var GrantedKv = class extends RpcTarget {
  #granted;
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the kv grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  async #call(op, body) {
    const resp = await this.#granted.fetch(`http://granted/kv/${op}`, {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/json" },
      body: JSON.stringify(toBridgeJson(body))
    });
    const value = await resp.json().catch(() => ({}));
    if (value && value.error) {
      throw PluginError.fromWire(value.error.code || "internal", value.error.message || "");
    }
    if (!resp.ok) {
      throw PluginError.fromWire("internal", `kv ${op} HTTP ${resp.status}`);
    }
    return value ?? {};
  }
  async get(key, type = "text") {
    const found = await this.#read(key, type);
    return found.value;
  }
  async getWithMetadata(key, type = "text") {
    return this.#read(key, type);
  }
  async #read(key, type) {
    const reply = await this.#call("get", { key: checkKvKey(key) });
    if (!reply.found)
      return { value: null, metadata: null };
    const bytes = typeof reply.value === "string" ? base64ToBytes(reply.value) : new Uint8Array();
    return { value: decodeKvValue(bytes, type), metadata: parseKvMetadata(reply.metadata) };
  }
  async put(key, value, options) {
    let bytes;
    if (typeof value === "string") {
      bytes = new TextEncoder().encode(value);
    } else if (value instanceof ArrayBuffer) {
      bytes = new Uint8Array(value);
    } else if (ArrayBuffer.isView(value)) {
      bytes = new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    } else {
      throw PluginError.fromWire("invalid_params", "kv value must be a string, ArrayBuffer, or view");
    }
    if (bytes.byteLength > MAX_KV_VALUE_BYTES) {
      throw PluginError.fromWire("payload_too_large", `kv value of ${bytes.byteLength} bytes exceeds ${MAX_KV_VALUE_BYTES}`);
    }
    const metadata = options?.metadata === void 0 ? "" : JSON.stringify(options.metadata);
    const metadataBytes = new TextEncoder().encode(metadata).byteLength;
    if (metadataBytes > MAX_KV_METADATA_BYTES) {
      throw PluginError.fromWire("payload_too_large", `kv metadata of ${metadataBytes} bytes exceeds ${MAX_KV_METADATA_BYTES}`);
    }
    const expiration = Math.max(0, Math.floor(Number(options?.expiration ?? 0) || 0));
    const ttl = Math.max(0, Math.floor(Number(options?.expirationTtl ?? 0) || 0));
    await this.#call("put", {
      key: checkKvKey(key),
      value: bytes,
      options: { expirationUnixMs: expiration * 1e3, expirationTtlSeconds: ttl, metadata }
    });
  }
  async delete(key) {
    await this.#call("delete", { key: checkKvKey(key) });
  }
  async list(options) {
    const limit = Math.max(0, Math.floor(Number(options?.limit ?? 0) || 0));
    if (limit > MAX_LIST_PAGE) {
      throw PluginError.fromWire("invalid_params", `kv list limit of ${limit} exceeds ${MAX_LIST_PAGE}`);
    }
    const reply = await this.#call("list", {
      options: { prefix: options?.prefix ?? "", cursor: options?.cursor ?? "", limit }
    });
    const keys = Array.isArray(reply.keys) ? reply.keys : [];
    const cursor = typeof reply.cursor === "string" && reply.cursor ? reply.cursor : void 0;
    return {
      keys: keys.map((entry) => {
        const ms = Number(entry.expirationUnixMs ?? 0) || 0;
        const metadata = parseKvMetadata(entry.metadata);
        return {
          name: String(entry.key ?? ""),
          ...ms > 0 ? { expiration: Math.floor(ms / 1e3) } : {},
          ...metadata !== null ? { metadata } : {}
        };
      }),
      list_complete: Boolean(reply.listComplete),
      ...cursor ? { cursor } : {}
    };
  }
};
var GrantedSockets = class extends RpcTarget {
  /** Granted reverse channel that serves `POST /sockets/<op>`. */
  #granted;
  /** `Authorization` header carrying the sockets-only grant token. */
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the sockets grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Makes one `POST /sockets/<op>` call on the granted channel.
   *
   * @param op - Operation, the path segment after `/sockets/`.
   * @param body - That operation's JSON fields, bytes already base64.
   * @returns `{ ok }` with the route's reply body, or `{ err }` with the
   *   host's code (`unavailable` when the channel cannot be reached,
   *   `internal` for a reply that is not JSON); never throws.
   */
  async call(op, body) {
    let resp;
    try {
      resp = await this.#granted.fetch(`http://granted/sockets/${op}`, {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/json" },
        body: JSON.stringify(body)
      });
    } catch (err) {
      return { err: { code: "unavailable", message: `sockets ${op}: ${errorMessage(err)}` } };
    }
    const value = await resp.json().catch(() => null);
    if (value && value.error) {
      return {
        err: { code: value.error.code || "internal", message: value.error.message || "" }
      };
    }
    if (!resp.ok || !value || typeof value !== "object") {
      return { err: { code: "internal", message: `sockets ${op} HTTP ${resp.status}` } };
    }
    return { ok: value };
  }
};
var GrantedOrigin = class extends RpcTarget {
  /** Granted reverse channel that serves `POST /origin/fetch`. */
  #granted;
  /** `Authorization` header carrying the origin-only grant token. */
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the origin grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Hands one request to the library.
   *
   * @param head - Method, target, header fields and client address.
   * @param body - Request body, or `null`.
   * @returns The library's response, or a typed failure; never throws.
   */
  fetch(head, body) {
    return grantedOriginFetch(this.#granted, this.#auth, head, body);
  }
  /**
   * Hands one WebSocket upgrade to the library.
   *
   * @param head - The upgrade's head.
   * @param attachment - The socket's initial attachment, base64.
   * @returns The route's answer, or a typed failure; never throws.
   */
  connect(head, attachment) {
    return grantedOriginConnect(this.#granted, this.#auth, head, attachment);
  }
};
var GrantedWebhooks = class extends RpcTarget {
  /** Granted reverse channel that serves `POST /webhooks/<op>`. */
  #granted;
  /** `Authorization` header carrying the webhooks-only grant token. */
  #auth;
  /**
   * @param granted - Granted reverse channel.
   * @param auth - Bearer header for the webhooks grant.
   */
  constructor(granted, auth) {
    super();
    this.#granted = granted;
    this.#auth = auth;
  }
  /**
   * Makes one `POST /webhooks/<op>` call on the granted channel.
   *
   * @param op - Operation, the path segment after `/webhooks/`.
   * @param body - That operation's JSON fields, bytes already base64.
   * @returns `{ ok }` with the route's reply body, or `{ err }` with the
   *   host's code (`unavailable` when the channel cannot be reached,
   *   `internal` for a reply that is not JSON); never throws.
   */
  async call(op, body) {
    let resp;
    try {
      resp = await this.#granted.fetch(`http://granted/webhooks/${op}`, {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/json" },
        body: JSON.stringify(body)
      });
    } catch (err) {
      return { err: { code: "unavailable", message: `webhooks ${op}: ${errorMessage(err)}` } };
    }
    const value = await resp.json().catch(() => null);
    if (value && value.error) {
      return {
        err: { code: value.error.code || "internal", message: value.error.message || "" }
      };
    }
    if (!resp.ok || !value || typeof value !== "object") {
      return { err: { code: "internal", message: `webhooks ${op} HTTP ${resp.status}` } };
    }
    return { ok: value };
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
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes
      });
      return await grantedDatabaseBody(resp);
    } catch (err) {
      return encodeExecuteResultReply({ err: wirePluginError(err) });
    }
  }
  /**
   * Run one typed atomic batch answered as stream frames
   * (`POST /db/execute-streamed`). A non-2xx status is the channel's error;
   * an adapter or authorization failure travels inside the frames.
   *
   * @param request - Structured `ExecuteRequest`.
   * @returns The response body: the frame sequence.
   */
  async executeStreamed(request) {
    const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
      method: "POST",
      headers: { ...this.#auth, "content-type": "application/octet-stream" },
      body: encodeExecuteRequest(request)
    });
    if (!resp.ok) {
      await grantedDatabaseBody(resp, "executeStreamed");
    }
    return resp.body ?? new Uint8Array(0);
  }
  /**
   * Same frames, whole, from Cap'n request bytes (Python authors decode
   * locally).
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns Every `ExecuteStreamFrame`, concatenated; a channel failure is
   *   one `err` frame.
   */
  async executeStreamedBytes(bytes) {
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes
      });
      return await grantedDatabaseBody(resp, "executeStreamed");
    } catch (err) {
      return errFrame(err);
    }
  }
  /**
   * Same frames, as the channel delivers them (a `ReadableStream` when it
   * streams), from Cap'n request bytes, so a Python `iterate()` decodes
   * frames as they arrive.
   *
   * @param bytes - Unpacked `ExecuteRequest` message.
   * @returns The response body: the frame sequence; a channel failure is
   *   one `err` frame.
   */
  async openStreamedBytes(bytes) {
    try {
      const resp = await this.#granted.fetch("http://granted/db/execute-streamed", {
        method: "POST",
        headers: { ...this.#auth, "content-type": "application/octet-stream" },
        body: bytes
      });
      if (!resp.ok) {
        await grantedDatabaseBody(resp, "executeStreamed");
      }
      return resp.body ?? new Uint8Array(0);
    } catch (err) {
      return errFrame(err);
    }
  }
  /**
   * Negotiated limits of the granted session (`GET /db/limits`).
   *
   * @returns The adapter's `DbCapabilities` for this binding.
   */
  async limits() {
    return decodeDbCapabilitiesReply(await this.limitsBytes());
  }
  /**
   * Same answer as Cap'n Proto bytes (Python authors decode locally).
   *
   * @returns Unpacked `DbCapabilitiesReply` message; a channel failure is
   *   its `err` arm.
   */
  async limitsBytes() {
    try {
      const resp = await this.#granted.fetch("http://granted/db/limits", {
        method: "GET",
        headers: { ...this.#auth }
      });
      return await grantedDatabaseBody(resp, "limits");
    } catch (err) {
      return encodeDbCapabilitiesReply({ err: wirePluginError(err) });
    }
  }
};
function wirePluginError(err) {
  if (err instanceof PluginError) {
    return { code: err.wireCode, message: err.message, details: err.detailsText };
  }
  const message = err instanceof Error ? err.message : String(err);
  return { code: "unavailable", message: `database grant: ${message}`, details: "" };
}
function errFrame(err) {
  return encodeMessage(ExecuteStreamFrameCodec, { kind: "err", value: wirePluginError(err) });
}
async function grantedDatabaseBody(resp, method = "execute") {
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    const message = `database grant: ${resp.status} ${text}`.trim();
    if (resp.status === 413) {
      const limit = parseLimitExceeded(text);
      throw limit === void 0 ? PluginError.fromWire("limit_exceeded", message) : new PluginError("limit_exceeded", message, { ...limit });
    }
    if (resp.status === 404 || resp.status === 405) {
      throw PluginError.fromWire("unsupported", `GuestDatabase.${method} is not served by this host (${message})`);
    }
    throw PluginError.fromWire(resp.status === 401 ? "unauthorized" : "unavailable", message);
  }
  return new Uint8Array(await resp.arrayBuffer());
}
var CancelWatch = class extends RpcTarget {
  #promise;
  constructor(granted, auth, stop) {
    super();
    this.#promise = new Promise((resolve) => {
      const poll = async () => {
        while (!stop.aborted) {
          const resp = await granted.fetch("http://granted/cancel", {
            headers: auth,
            signal: stop
          });
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
  queueConsumer: "queueConsumer",
  scheduledHandler: "scheduledHandler",
  jobRunner: "jobRunner",
  socketHandler: "socketHandler",
  webhookHandler: "webhookHandler"
});
function scheduledControllerOf(wire) {
  const w = wire && typeof wire === "object" ? wire : {};
  return Object.freeze({
    cron: String(w.cron ?? ""),
    scheduledTime: Number(w.scheduledTime ?? 0) || 0,
    fireId: String(w.fireId ?? ""),
    attempt: Number(w.attempt ?? 0) || 0,
    deadlineUnixMs: Number(w.deadlineUnixMs ?? 0) || 0
  });
}
function allowedEntrypointFamilies(capabilities, requested) {
  if (!capabilities || typeof capabilities !== "object")
    return [];
  const entrypoints = Array.isArray(capabilities.entrypoints) ? capabilities.entrypoints : [];
  const consumes = Array.isArray(capabilities.consumes) ? capabilities.consumes : [];
  const queueConsumers = Array.isArray(capabilities.queueConsumers) ? capabilities.queueConsumers : [];
  const jobs = Array.isArray(capabilities.jobs) ? capabilities.jobs : [];
  const crons = Array.isArray(capabilities.crons) ? capabilities.crons : [];
  const exportsStorage = entrypoints.includes("storage");
  const out = [];
  for (const name of requested) {
    if (typeof name !== "string" || out.includes(name))
      continue;
    if (name === TRIGGER_FAMILIES.eventConsumer) {
      if (consumes.length > 0)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.queueConsumer) {
      if (queueConsumers.length > 0)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.scheduledHandler) {
      if (crons.length > 0)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.jobRunner) {
      if (jobs.length > 0 || exportsStorage)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.socketHandler) {
      if (capabilities.sockets === true)
        out.push(name);
    } else if (name === TRIGGER_FAMILIES.webhookHandler) {
      if (capabilities.webhooks === true)
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
  const guestOptions = guest.configOptions;
  const configOptions = Array.isArray(guestOptions) && guestOptions.length > 0 ? guestOptions : base.configOptions ?? guestOptions;
  return {
    ...base,
    ...guest,
    apiVersion: base.apiVersion ?? guest.apiVersion ?? 3,
    id: base.id ?? guest.id,
    capabilities: base.capabilities ?? guest.capabilities,
    // `[media]` is the authority for the declared media, like capabilities.
    media: base.media ?? guest.media,
    ...cli === void 0 ? {} : { cli },
    ...configOptions === void 0 ? {} : { configOptions }
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
     * Turn the bridge context into the author-facing one: the host's events,
     * database, and queue grant tokens become `EVENTS` / `env.<BINDING>`
     * stubs and never reach the author.
     *
     * @param ctx - Bridge context from the launcher.
     * @returns Author-facing granted context.
     */
    #bindContext(ctx) {
      const source = ctx && typeof ctx === "object" ? ctx : {};
      const { eventsToken, notifyToken, aiToken, databases, queues, kv, sockets, originToken, webhooksToken, ...rest } = source;
      const bound = rest;
      const granted = this.env.GRANTED;
      if (typeof eventsToken === "string" && eventsToken && granted) {
        bound.events = new GrantedEvents(granted, {
          Authorization: `Bearer ${eventsToken}`
        });
      }
      if (typeof notifyToken === "string" && notifyToken && granted) {
        bound.notify = new GrantedNotify(granted, {
          Authorization: `Bearer ${notifyToken}`
        });
      }
      if (typeof aiToken === "string" && aiToken && granted) {
        bound.ai = new GrantedAi(granted, {
          Authorization: `Bearer ${aiToken}`
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
      if (queues && typeof queues === "object" && !Array.isArray(queues) && granted) {
        const named2 = [];
        for (const [name, token] of Object.entries(queues)) {
          if (!name || typeof token !== "string" || !token)
            continue;
          named2.push({
            name,
            queue: new GrantedQueue(granted, { Authorization: `Bearer ${token}` })
          });
        }
        if (named2.length > 0)
          bound.queues = named2;
      }
      if (kv && typeof kv === "object" && !Array.isArray(kv) && granted) {
        const named2 = [];
        for (const [name, token] of Object.entries(kv)) {
          if (!name || typeof token !== "string" || !token)
            continue;
          named2.push({ name, kv: new GrantedKv(granted, { Authorization: `Bearer ${token}` }) });
        }
        if (named2.length > 0)
          bound.kv = named2;
      }
      if (sockets && typeof sockets === "object" && !Array.isArray(sockets) && granted) {
        const named2 = [];
        for (const [name, token] of Object.entries(sockets)) {
          if (!name || typeof token !== "string" || !token)
            continue;
          named2.push({
            name,
            sockets: new GrantedSockets(granted, { Authorization: `Bearer ${token}` })
          });
        }
        if (named2.length > 0)
          bound.sockets = named2.slice(0, 1);
      }
      if (typeof originToken === "string" && originToken && granted) {
        bound.origin = new GrantedOrigin(granted, { Authorization: `Bearer ${originToken}` });
      }
      if (typeof webhooksToken === "string" && webhooksToken && granted) {
        bound.webhooks = new GrantedWebhooks(granted, {
          Authorization: `Bearer ${webhooksToken}`
        });
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
          return this.invokeEntrypoint("storage", ctx, "get", [
            String(args.key ?? ""),
            args.options
          ]);
        case "put": {
          if (!body) {
            throw PluginError.fromWire("invalid_params", "put missing body stream");
          }
          const options = args.options;
          const bounded = exactLengthBody(body, options?.contentLength);
          return this.invokeEntrypoint("storage", ctx, "put", [
            String(args.key ?? ""),
            bounded,
            options
          ]);
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

// dist/db-adapter.js
var GUEST_RECEIPT_WRAP_PREFIX = 2;
var GUEST_RECEIPT_STUB_SUFFIX = 1;
var GUEST_REPLAY_PAYLOAD_MAX_BYTES = 65536;
var GUEST_READ_ONLY_REPLAY_PAYLOAD = '{"readOnly":true}';
var GUEST_RECEIPT_RESULT_LOST = "guest receipt committed without payload; original response lost";
var GUEST_RECEIPT_CLAIM_LOST = "guest receipt claimed by a concurrent attempt; retry to replay it";
var STREAM_FRAME_TARGET_BYTES = 64 * 1024;
var STATUS_CLAIMED = "claimed";
var FINALIZE_SQL = "UPDATE bookclerk_receipts SET payload = ?, status = 'ok' WHERE operation_id = ? AND status IN ('claimed', 'applied')";
async function executeAdapterBatch(request, begin, options) {
  return inTransaction(begin, (txn) => runAdapterBatch(request, txn, options));
}
async function executeAdapterBatchStreamed(request, begin, options) {
  return inTransaction(begin, async (txn) => {
    const reply = await runAdapterBatch(request, txn, options);
    const frames = encodeExecuteStream(reply, STREAM_FRAME_TARGET_BYTES, MAX_SCALAR_BYTES);
    decodeExecuteStream(frames, options.limits);
    requireReplyWithinBudget(reply, options.limits);
    return frames;
  });
}
function requireReplyWithinBudget(reply, limits) {
  if (limits.maxBytes === 0) {
    return;
  }
  const used = encodeExecuteReply(reply).length;
  if (used > limits.maxBytes) {
    throw PluginError.limitExceeded({ limit: limits.bytesLimit, cap: limits.maxBytes, used });
  }
}
async function inTransaction(begin, body) {
  const txn = await begin();
  let result;
  try {
    result = await body(txn);
    await txn.commit();
  } catch (err) {
    try {
      await txn.rollback();
    } catch {
    }
    throw err;
  }
  return result;
}
async function runAdapterBatch(request, txn, options) {
  const started = Date.now();
  const total = request.statements.length;
  const guestLen = request.receipt.guestLen;
  const guestHash = request.receipt.guestHash;
  const statements = [];
  for (const statement of request.statements) {
    statements.push(await txn.run(statement));
    if (guestLen > 0 && shouldSkipRemainingGuestWork(statements, total, guestHash)) {
      while (statements.length < total) {
        statements.push(affected(0));
      }
      break;
    }
  }
  const timing = () => {
    const elapsed = Math.max(0, Date.now() - started) * 1e3;
    return {
      attemptElapsedUs: elapsed,
      dbExecutionUs: elapsed,
      dbTimingSource: options.timingSource
    };
  };
  for (const result of statements) {
    for (const row of result.rows) {
      row.values.forEach(validateCell);
    }
  }
  if (guestLen > 0) {
    const prior = statements[1];
    const stub = statements[statements.length - 1];
    if (prior !== void 0 && prior.rows.length === 0 && stub?.rowsAffected === 0) {
      throw PluginError.fromWire("unavailable", GUEST_RECEIPT_CLAIM_LOST);
    }
    const partial = {
      operationId: request.operationId,
      statements,
      timing: timing()
    };
    const readOnly = guestBatchIsReadOnly(request.statements, guestLen);
    for (const statement of guestReceiptFinalizeStatements(partial, guestLen, guestHash, readOnly)) {
      await txn.run(statement);
    }
  }
  return { operationId: request.operationId, statements, timing: timing() };
}
function guestBatchIsReadOnly(statements, guestLen) {
  const end = GUEST_RECEIPT_WRAP_PREFIX + guestLen;
  if (guestLen === 0 || end > statements.length) {
    return false;
  }
  return statements.slice(GUEST_RECEIPT_WRAP_PREFIX, end).every((s) => s.kind === "select");
}
function guestReceiptFinalizeStatements(partial, guestLen, guestHash, readOnly) {
  const expected = GUEST_RECEIPT_WRAP_PREFIX + guestLen + GUEST_RECEIPT_STUB_SUFFIX;
  if (partial.statements.length !== expected) {
    throw PluginError.fromWire("internal", `guest atomic receipt wrap returned ${partial.statements.length} statements; expected ${expected}`);
  }
  const prior = partial.statements[1];
  const priorHash = prior ? receiptHash(prior) : void 0;
  if (prior && priorHash !== void 0) {
    if (priorHash !== guestHash) {
      return [];
    }
    if (receiptText(prior, "payload", 3) !== void 0) {
      return [];
    }
    const claimed = receiptText(prior, "status", 2) === STATUS_CLAIMED;
    const stubAffected = partial.statements[partial.statements.length - 1]?.rowsAffected ?? 0;
    if (!claimed && stubAffected === 0) {
      throw PluginError.fromWire("unavailable", GUEST_RECEIPT_RESULT_LOST);
    }
    if (claimed && stubAffected === 0) {
      return [];
    }
  }
  const payload = readOnly ? GUEST_READ_ONLY_REPLAY_PAYLOAD : guestReplayPayload({
    operationId: partial.operationId,
    statements: partial.statements.slice(GUEST_RECEIPT_WRAP_PREFIX, GUEST_RECEIPT_WRAP_PREFIX + guestLen),
    timing: partial.timing
  });
  return [
    {
      sql: FINALIZE_SQL,
      parameters: [
        { kind: "text", value: payload },
        { kind: "text", value: partial.operationId }
      ],
      kind: "execute",
      maxRows: 0,
      resultSelection: "affectedRows"
    }
  ];
}
function shouldSkipRemainingGuestWork(results, total, guestHash) {
  if (results.length !== GUEST_RECEIPT_WRAP_PREFIX || total <= GUEST_RECEIPT_WRAP_PREFIX) {
    return false;
  }
  const prior = results[1];
  const priorHash = prior ? receiptHash(prior) : void 0;
  if (!prior || priorHash === void 0) {
    return false;
  }
  const sameHash = priorHash === guestHash;
  const resumesClaim = sameHash && receiptText(prior, "status", 2) === STATUS_CLAIMED;
  const readOnlyReplay = sameHash && receiptText(prior, "payload", 3) === GUEST_READ_ONLY_REPLAY_PAYLOAD;
  return !resumesClaim && !readOnlyReplay;
}
function guestReplayPayload(reply) {
  const statements = reply.statements.map(statementJson).join(",");
  const t = reply.timing;
  const text = `{"operation_id":${JSON.stringify(reply.operationId)},"statements":[${statements}],"timing":{"attemptElapsedUs":${uint(t.attemptElapsedUs)},"dbExecutionUs":${uint(t.dbExecutionUs)},"dbTimingSource":${JSON.stringify(t.dbTimingSource)}}}`;
  const bytes = new TextEncoder().encode(text).byteLength;
  if (bytes > GUEST_REPLAY_PAYLOAD_MAX_BYTES) {
    throw PluginError.fromWire("internal", `guest replay payload is ${bytes} bytes; max is ${GUEST_REPLAY_PAYLOAD_MAX_BYTES}`);
  }
  return text;
}
function encodeExecuteStream(reply, frameTargetBytes = STREAM_FRAME_TARGET_BYTES, maxBytes = Number.POSITIVE_INFINITY) {
  const parts = [];
  let total = 0;
  const push = (part) => {
    total += part.byteLength;
    if (total > maxBytes) {
      throw PluginError.limitExceeded({ limit: "maxScalarBytes", cap: maxBytes, used: total });
    }
    parts.push(part);
  };
  push(frame({
    kind: "head",
    value: { operationId: reply.operationId, statementCount: reply.statements.length }
  }));
  reply.statements.forEach((stmt, index) => {
    push(frame({
      kind: "statement",
      value: { index, columns: stmt.columns, rowsAffected: stmt.rowsAffected }
    }));
    let start = 0;
    let size = 0;
    stmt.rows.forEach((row, i) => {
      const estimate = rowSizeEstimate(row);
      if (size > 0 && size + estimate > frameTargetBytes) {
        pushRowsFrames(push, stmt.rows.slice(start, i));
        start = i;
        size = 0;
      }
      size += estimate;
    });
    pushRowsFrames(push, stmt.rows.slice(start));
  });
  push(frame({ kind: "end", value: reply.timing }));
  const out = new Uint8Array(total);
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.byteLength;
  }
  return out;
}
function pushRowsFrames(push, rows) {
  if (rows.length === 0) {
    return;
  }
  const encoded = frame({ kind: "rows", value: rows });
  if (encoded.byteLength > MAX_SCALAR_BYTES && rows.length > 1) {
    const mid = Math.floor(rows.length / 2);
    pushRowsFrames(push, rows.slice(0, mid));
    pushRowsFrames(push, rows.slice(mid));
    return;
  }
  push(encoded);
}
function frame(value) {
  return encodeMessage(ExecuteStreamFrameCodec, value);
}
function rowSizeEstimate(row) {
  let size = 16;
  for (const value of row.values) {
    if (value.kind === "text") {
      size += new TextEncoder().encode(value.value).byteLength + 24;
    } else if (value.kind === "bytes") {
      size += value.value.byteLength + 24;
    } else {
      size += 24;
    }
  }
  return size;
}
function receiptHash(prior) {
  const cell = receiptCell(prior, "request_hash", 1);
  return cell?.kind === "text" ? cell.value : void 0;
}
function receiptText(prior, column, fallback) {
  const cell = receiptCell(prior, column, fallback);
  return cell?.kind === "text" && cell.value !== "" ? cell.value : void 0;
}
function receiptCell(prior, column, fallback) {
  const row = prior.rows[0];
  if (!row) {
    return void 0;
  }
  const named2 = prior.columns.findIndex((c) => c.name.toLowerCase() === column);
  return row.values[named2 >= 0 ? named2 : fallback];
}
function affected(n) {
  return { rows: [], columns: [], rowsAffected: n };
}
function statementJson(stmt) {
  const rows = stmt.rows.map((row) => `{"values":[${row.values.map(valueJson).join(",")}]}`);
  const columns = stmt.columns.map(columnJson);
  return `{"rows":[${rows.join(",")}],"columns":[${columns.join(",")}],"rowsAffected":${uint(stmt.rowsAffected)}}`;
}
function columnJson(column) {
  return `{"name":${JSON.stringify(column.name)},"dbType":${JSON.stringify(column.dbType)}}`;
}
function valueJson(value) {
  switch (value.kind) {
    case "null":
      return `{"kind":"null","value":${JSON.stringify(value.value)}}`;
    case "boolean":
      return `{"kind":"boolean","value":${value.value ? "true" : "false"}}`;
    case "int64":
      return `{"kind":"int64","value":${BigInt(value.value).toString()}}`;
    case "float64":
      if (!Number.isFinite(value.value)) {
        throw PluginError.fromWire("internal", "guest replay payload has a non-finite float");
      }
      return `{"kind":"float64","value":${Object.is(value.value, -0) ? "-0.0" : JSON.stringify(value.value)}}`;
    case "text":
      return `{"kind":"text","value":${JSON.stringify(value.value)}}`;
    case "bytes":
      return `{"kind":"bytes","value":[${Array.from(value.value).join(",")}]}`;
  }
}
function uint(n) {
  if (!Number.isSafeInteger(n) || n < 0) {
    throw PluginError.fromWire("internal", `guest replay payload counter ${n} is not a u64`);
  }
  return String(n);
}

// dist/scan-page.js
var CURSOR_PREFIX = "sp1.";
var UTF82 = new TextEncoder();
function writeCursor(position) {
  const cursor = `${CURSOR_PREFIX}${position.offset}.${position.storeCursor}`;
  if (UTF82.encode(cursor).length > MAX_SCAN_CURSOR_BYTES) {
    throw new PluginError("internal", `store cursor too long: a scan cursor is at most ${MAX_SCAN_CURSOR_BYTES} bytes`);
  }
  return cursor;
}
function readCursor(cursor) {
  if (cursor === "")
    return { storeCursor: "", offset: 0 };
  const stale = () => new PluginError("invalid_cursor", "scan cursor was not written by scanPage");
  if (!cursor.startsWith(CURSOR_PREFIX))
    throw stale();
  const rest = cursor.slice(CURSOR_PREFIX.length);
  const dot = rest.indexOf(".");
  const digits = dot < 0 ? "" : rest.slice(0, dot);
  if (!/^(0|[1-9][0-9]{0,15})$/.test(digits))
    throw stale();
  const offset = Number(digits);
  if (!Number.isSafeInteger(offset) || offset % MAX_SCAN_PAGE_BOOKS !== 0)
    throw stale();
  return { storeCursor: rest.slice(dot + 1), offset };
}
async function scanPage(params, fetchStorePage) {
  const { storeCursor, offset } = readCursor(params.cursor);
  const page = await fetchStorePage(storeCursor, params);
  const removed = page.removed ?? [];
  if (offset > 0 && offset >= page.books.length && offset >= removed.length) {
    throw new PluginError("invalid_cursor", "the store page shrank during the pass");
  }
  const end = offset + MAX_SCAN_PAGE_BOOKS;
  const reply = {
    books: page.books.slice(offset, end),
    removed: removed.slice(offset, end),
    nextCursor: "",
    syncToken: "",
    complete: false,
    storePages: page.storePages ?? 1
  };
  if (page.books.length > end || removed.length > end) {
    reply.nextCursor = writeCursor({ storeCursor, offset: end });
  } else if (page.next) {
    reply.nextCursor = writeCursor({ storeCursor: page.next, offset: 0 });
  } else {
    reply.syncToken = page.syncToken ?? "";
    reply.complete = page.complete ?? false;
  }
  return reply;
}
export {
  AdapterDatabaseSession,
  AiEntrypoint,
  BookclerkEntrypoint,
  CliEntrypoint,
  CoverEntrypoint,
  DatabaseAdapterEntrypoint,
  Destination,
  ENTRYPOINT_BINDINGS,
  ENTRYPOINT_CLASSES,
  EventBatch,
  EventMessage,
  ExecuteStreamDecoder,
  FEATURE_SCALAR_LIMITS,
  FEATURE_STORAGE_CHANGES,
  FEATURE_STORAGE_CONDITIONAL_WRITE,
  FEATURE_STORAGE_COPY,
  FEATURE_STORAGE_DELETE_MANY,
  FEATURE_STORAGE_HEAD_MANY,
  FEATURE_STORAGE_MOVE,
  FEATURE_STORAGE_MOVE_MANY,
  FEATURE_STREAMS,
  GUEST_READ_ONLY_REPLAY_PAYLOAD,
  GUEST_RECEIPT_CLAIM_LOST,
  GUEST_RECEIPT_RESULT_LOST,
  GUEST_RECEIPT_STUB_SUFFIX,
  GUEST_RECEIPT_WRAP_PREFIX,
  GUEST_REPLAY_PAYLOAD_MAX_BYTES,
  IngressEntrypoint,
  InvokeError,
  JobController,
  MAX_BATCH_KEYS,
  MAX_CHANGES_CURSOR_BYTES,
  MAX_CHANGES_PAGE,
  MAX_CHECKPOINT_BYTES,
  MAX_EVENT_PAYLOAD_BYTES,
  MAX_LIST_PAGE,
  MAX_PLUGIN_MIGRATION_OPS,
  MAX_PLUGIN_MIGRATION_REGISTRATION_BYTES,
  MAX_PLUGIN_MIGRATION_TOTAL_OPS,
  MAX_SCALAR_BYTES,
  MAX_STREAM_WINDOW_BYTES,
  MAX_WRITE_KEY_BYTES,
  MetadataEntrypoint,
  MetadataFilesEntrypoint,
  NamedEntrypoint,
  NotifierEntrypoint,
  OidcEntrypoint,
  PRODUCT_API_VERSION,
  PluginError,
  ProgressSink,
  QueueBatch,
  QueueMessage,
  RemoteLibraryEntrypoint,
  STREAM_FRAME_TARGET_BYTES,
  Source,
  StorageEntrypoint,
  StorefrontEntrypoint,
  TRIGGER_FAMILIES,
  WriteChunksError,
  allowedEntrypointFamilies,
  canonicalExecuteRequestHash,
  chunkIndexes,
  cliArgs,
  collectExecuteStream,
  createDatabaseBinding,
  dataMigrationOp,
  decodeDbValue,
  decodeExecuteRequest,
  decodeExecuteResultReply,
  decodeExecuteStream,
  decodeExtensibleConfig,
  dispatchInvoke,
  encodeDbValue,
  encodeExecuteRequest,
  encodeExecuteResultReply,
  encodeExecuteStream,
  eventBatchResults,
  executeAdapterBatch,
  executeAdapterBatchStreamed,
  executeReplyToD1Results,
  executeStreamLimits,
  fromBridgeJson,
  guestBatchIsReadOnly,
  guestReceiptFinalizeStatements,
  guestReplayPayload,
  invocationOf,
  jobOutcomeFor,
  jsonPayload,
  packEmbeddingVectors,
  parseDbValue,
  queueBatchResults,
  rawStreamCeiling,
  requirePluginMigrationRegistration,
  runAdapterBatch,
  scanPage,
  schemaMigrationOp,
  shouldSkipRemainingGuestWork,
  statementResultToD1Result,
  streamsResults,
  toBridgeJson,
  unpackEmbeddingVectors,
  wrapPluginFromBinding,
  wrapPluginFromNative,
  writeChunksOperationId
};
