/**
 * Unpacked single-segment Cap'n Proto builders and readers.
 *
 * Not a public SDK export. Runtime under the generated `generated-wire.ts`
 * codecs and the hand-written `db-execute.ts` helpers. Encodes the
 * `write_message` single-segment stream format produced by `capnpc-rust`;
 * far pointers and multi-segment messages are rejected. Reads follow Cap'n
 * Proto evolution semantics: fields beyond the actual struct size decode as
 * their zero default and a null struct pointer decodes as an all-default
 * struct.
 *
 * @internal
 * @module
 */

const WORD = 8;
const MAX_TRAVERSAL_WORDS = 64 * 1024;

/**
 * Transport capability table for interface-typed fields.
 *
 * A codec never sees the transport; it hands capability values to the table
 * and stores the returned index in the message (and vice versa on read).
 *
 * @internal
 */
export interface CapTable {
  /** Registers `value` and returns its capability index. */
  exportCap(value: unknown): number;
  /** Resolves a capability index (`null` for a null pointer). */
  importCap(index: number | null): unknown;
}

/**
 * Capability table for messages that carry no interface fields.
 *
 * @internal
 */
export const NO_CAPS: CapTable = {
  exportCap(): number {
    throw new Error("message carries a capability but no CapTable was provided");
  },
  importCap(index: number | null): unknown {
    if (index === null) {
      return null;
    }
    throw new Error("message carries a capability but no CapTable was provided");
  },
};

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

export class CapnpMessage {
  private buf: Uint8Array;
  private view: DataView;
  /** Words used in the segment, including the root pointer at word 0. */
  usedWords = 1;

  constructor() {
    this.buf = new Uint8Array(256);
    this.view = new DataView(this.buf.buffer, this.buf.byteOffset, this.buf.byteLength);
  }

  alloc(nWords: number): number {
    const off = this.usedWords;
    this.usedWords += nWords;
    this.ensure((this.usedWords + 1) * WORD);
    return off;
  }

  initRoot(dataWords: number, pointerWords: number): CapnpStruct {
    const off = this.alloc(dataWords + pointerWords);
    this.writeStructPointer(0, off, dataWords, pointerWords);
    return new CapnpStruct(this, off, dataWords, pointerWords);
  }

  finish(): Uint8Array {
    const segBytes = this.usedWords * WORD;
    const out = new Uint8Array(WORD + segBytes);
    const view = new DataView(out.buffer);
    view.setUint32(0, 0, true);
    view.setUint32(4, this.usedWords, true);
    out.set(this.buf.subarray(0, segBytes), WORD);
    return out;
  }

  writeStructPointer(
    ptrWord: number,
    targetWord: number,
    dataWords: number,
    pointerWords: number,
  ): void {
    const offset = targetWord - (ptrWord + 1);
    const word =
      0n |
      (BigInt(offset & 0x3fffffff) << 2n) |
      (BigInt(dataWords) << 32n) |
      (BigInt(pointerWords) << 48n);
    this.setWord(ptrWord, word);
  }

  writeListPointer(
    ptrWord: number,
    targetWord: number,
    elementSize: number,
    listLength: number,
  ): void {
    const offset = targetWord - (ptrWord + 1);
    const word =
      1n |
      (BigInt(offset & 0x3fffffff) << 2n) |
      (BigInt(elementSize) << 32n) |
      (BigInt(listLength) << 35n);
    this.setWord(ptrWord, word);
  }

  writeCapPointer(ptrWord: number, index: number): void {
    this.setWord(ptrWord, 3n | (BigInt(index) << 32n));
  }

  writeEmptyCompositeList(
    ptrWord: number,
    dataWords: number,
    pointerWords: number,
  ): void {
    const tagWord = this.alloc(1);
    this.writeListPointer(ptrWord, tagWord, 7, 0);
    const tag =
      0n | (BigInt(dataWords) << 32n) | (BigInt(pointerWords) << 48n);
    this.setWord(tagWord, tag);
  }

  initStructList(
    ptrWord: number,
    count: number,
    dataWords: number,
    pointerWords: number,
  ): CapnpStruct[] {
    if (count === 0) {
      this.writeEmptyCompositeList(ptrWord, dataWords, pointerWords);
      return [];
    }
    const elemWords = dataWords + pointerWords;
    const payloadWords = count * elemWords;
    const tagWord = this.alloc(1 + payloadWords);
    this.writeListPointer(ptrWord, tagWord, 7, payloadWords);
    const tag =
      0n |
      (BigInt(count) << 2n) |
      (BigInt(dataWords) << 32n) |
      (BigInt(pointerWords) << 48n);
    this.setWord(tagWord, tag);
    const out: CapnpStruct[] = [];
    for (let i = 0; i < count; i++) {
      out.push(
        new CapnpStruct(this, tagWord + 1 + i * elemWords, dataWords, pointerWords),
      );
    }
    return out;
  }

  setText(ptrWord: number, value: string): void {
    const encoded = textEncoder.encode(value);
    const withNul = new Uint8Array(encoded.length + 1);
    withNul.set(encoded, 0);
    this.setByteList(ptrWord, withNul);
  }

  setData(ptrWord: number, value: Uint8Array): void {
    this.setByteList(ptrWord, value);
  }

  setByteList(ptrWord: number, bytes: Uint8Array): void {
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
  setPointerList(ptrWord: number, count: number): number[] {
    if (count === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 6, 0);
      return [];
    }
    const target = this.alloc(count);
    this.writeListPointer(ptrWord, target, 6, count);
    const out: number[] = [];
    for (let i = 0; i < count; i++) {
      out.push(target + i);
    }
    return out;
  }

  setTextList(ptrWord: number, values: readonly string[]): void {
    const words = this.setPointerList(ptrWord, values.length);
    for (let i = 0; i < values.length; i++) {
      this.setText(words[i]!, values[i]!);
    }
  }

  setDataList(ptrWord: number, values: readonly Uint8Array[]): void {
    const words = this.setPointerList(ptrWord, values.length);
    for (let i = 0; i < values.length; i++) {
      this.setData(words[i]!, values[i]!);
    }
  }

  setBoolList(ptrWord: number, values: readonly boolean[]): void {
    const count = values.length;
    if (count === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 1, 0);
      return;
    }
    const target = this.alloc(Math.ceil(count / 64));
    const base = target * WORD;
    for (let i = 0; i < count; i++) {
      if (values[i]) {
        this.buf[base + (i >> 3)]! |= 1 << (i & 7);
      }
    }
    this.writeListPointer(ptrWord, target, 1, count);
  }

  setUint16(word: number, fieldIndex: number, value: number): void {
    this.view.setUint16(word * WORD + fieldIndex * 2, value, true);
  }

  setInt32(word: number, fieldIndex: number, value: number): void {
    this.view.setInt32(word * WORD + fieldIndex * 4, value | 0, true);
  }

  setUint32(word: number, fieldIndex: number, value: number): void {
    this.view.setUint32(word * WORD + fieldIndex * 4, value >>> 0, true);
  }

  setInt64(word: number, fieldIndex: number, value: bigint): void {
    this.view.setBigInt64(word * WORD + fieldIndex * 8, value, true);
  }

  setUint64(word: number, fieldIndex: number, value: bigint): void {
    this.view.setBigUint64(word * WORD + fieldIndex * 8, value, true);
  }

  setFloat32(word: number, fieldIndex: number, value: number): void {
    this.view.setFloat32(word * WORD + fieldIndex * 4, value, true);
  }

  setFloat64(word: number, fieldIndex: number, value: number): void {
    this.view.setFloat64(word * WORD + fieldIndex * 8, value, true);
  }

  setBool(word: number, bitIndex: number, value: boolean): void {
    const byteOff = word * WORD + (bitIndex >> 3);
    const mask = 1 << (bitIndex & 7);
    if (value) {
      this.buf[byteOff]! |= mask;
    } else {
      this.buf[byteOff]! &= ~mask;
    }
  }

  setWord(word: number, value: bigint): void {
    this.view.setBigUint64(word * WORD, value, true);
  }

  private ensure(bytes: number): void {
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
}

export class CapnpStruct {
  constructor(
    readonly msg: CapnpMessage,
    readonly word: number,
    readonly dataWords: number,
    readonly pointerWords: number,
  ) {}

  pointerWord(index: number): number {
    if (index >= this.pointerWords) {
      throw new Error("pointer index outside struct pointer section");
    }
    return this.word + this.dataWords + index;
  }

  setUint16(fieldIndex: number, value: number): void {
    this.msg.setUint16(this.word, fieldIndex, value);
  }

  setInt32(fieldIndex: number, value: number): void {
    this.msg.setInt32(this.word, fieldIndex, value);
  }

  setUint32(fieldIndex: number, value: number): void {
    this.msg.setUint32(this.word, fieldIndex, value);
  }

  setInt64(fieldIndex: number, value: bigint): void {
    this.msg.setInt64(this.word, fieldIndex, value);
  }

  setUint64(fieldIndex: number, value: bigint): void {
    this.msg.setUint64(this.word, fieldIndex, value);
  }

  setFloat32(fieldIndex: number, value: number): void {
    this.msg.setFloat32(this.word, fieldIndex, value);
  }

  setFloat64(fieldIndex: number, value: number): void {
    this.msg.setFloat64(this.word, fieldIndex, value);
  }

  setBool(bitIndex: number, value: boolean): void {
    this.msg.setBool(this.word, bitIndex, value);
  }

  setText(pointerIndex: number, value: string): void {
    this.msg.setText(this.pointerWord(pointerIndex), value);
  }

  setData(pointerIndex: number, value: Uint8Array): void {
    this.msg.setData(this.pointerWord(pointerIndex), value);
  }

  setCap(pointerIndex: number, index: number): void {
    this.msg.writeCapPointer(this.pointerWord(pointerIndex), index);
  }

  setTextList(pointerIndex: number, values: readonly string[]): void {
    this.msg.setTextList(this.pointerWord(pointerIndex), values);
  }

  setDataList(pointerIndex: number, values: readonly Uint8Array[]): void {
    this.msg.setDataList(this.pointerWord(pointerIndex), values);
  }

  setBoolList(pointerIndex: number, values: readonly boolean[]): void {
    this.msg.setBoolList(this.pointerWord(pointerIndex), values);
  }

  initStructList(
    pointerIndex: number,
    count: number,
    dataWords: number,
    pointerWords: number,
  ): CapnpStruct[] {
    return this.msg.initStructList(
      this.pointerWord(pointerIndex),
      count,
      dataWords,
      pointerWords,
    );
  }

  initStruct(
    pointerIndex: number,
    dataWords: number,
    pointerWords: number,
  ): CapnpStruct {
    const ptrWord = this.pointerWord(pointerIndex);
    const off = this.msg.alloc(dataWords + pointerWords);
    this.msg.writeStructPointer(ptrWord, off, dataWords, pointerWords);
    return new CapnpStruct(this.msg, off, dataWords, pointerWords);
  }
}

interface ListPointer {
  target: number;
  length: number;
}

export class CapnpReader {
  private readonly view: DataView;
  private readonly segOff: number;
  private readonly size0: number;

  constructor(bytes: Uint8Array) {
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

  root(dataWords: number, pointerWords: number): StructReader {
    return this.structAt(0, dataWords, pointerWords);
  }

  structAt(ptrWord: number, dataWords: number, pointerWords: number): StructReader {
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
    const offset = signExtend30(Number((word >> 2n) & 0x3fffffffn));
    const dw = Number((word >> 32n) & 0xffffn);
    const pw = Number((word >> 48n) & 0xffffn);
    const target = ptrWord + 1 + offset;
    this.checkRange(target, dw + pw);
    return new StructReader(this, target, dw, pw);
  }

  readWord(word: number): bigint {
    this.checkRange(word, 1);
    return this.view.getBigUint64(this.segOff + word * WORD, true);
  }

  private checkRange(word: number, nWords: number): void {
    if (word < 0 || nWords < 0 || word + nWords > this.size0) {
      throw new Error("Cap'n pointer out of segment");
    }
  }

  private listPointer(ptrWord: number, expectSize: number): ListPointer | null {
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
    const offset = signExtend30(Number((word >> 2n) & 0x3fffffffn));
    const c = Number((word >> 32n) & 7n);
    const d = Number(word >> 35n);
    if (c !== expectSize) {
      throw new Error(`expected list element size ${expectSize}, found ${c}`);
    }
    return { target: ptrWord + 1 + offset, length: d };
  }

  getUint16(word: number, fieldIndex: number): number {
    return this.view.getUint16(this.segOff + word * WORD + fieldIndex * 2, true);
  }

  getInt32(word: number, fieldIndex: number): number {
    return this.view.getInt32(this.segOff + word * WORD + fieldIndex * 4, true);
  }

  getUint32(word: number, fieldIndex: number): number {
    return this.view.getUint32(this.segOff + word * WORD + fieldIndex * 4, true);
  }

  getInt64(word: number, fieldIndex: number): bigint {
    return this.view.getBigInt64(this.segOff + word * WORD + fieldIndex * 8, true);
  }

  getUint64(word: number, fieldIndex: number): bigint {
    return this.view.getBigUint64(this.segOff + word * WORD + fieldIndex * 8, true);
  }

  getFloat32(word: number, fieldIndex: number): number {
    return this.view.getFloat32(this.segOff + word * WORD + fieldIndex * 4, true);
  }

  getFloat64(word: number, fieldIndex: number): number {
    return this.view.getFloat64(this.segOff + word * WORD + fieldIndex * 8, true);
  }

  getBool(word: number, bitIndex: number): boolean {
    const byteOff = this.segOff + word * WORD + (bitIndex >> 3);
    return (this.view.getUint8(byteOff) & (1 << (bitIndex & 7))) !== 0;
  }

  readByteList(ptrWord: number): Uint8Array {
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

  readText(ptrWord: number): string {
    const bytes = this.readByteList(ptrWord);
    const end = bytes.length > 0 && bytes[bytes.length - 1] === 0 ? bytes.length - 1 : bytes.length;
    return textDecoder.decode(bytes.subarray(0, end));
  }

  readCapIndex(ptrWord: number): number | null {
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return null;
    }
    if ((word & 3n) !== 3n) {
      throw new Error("expected capability pointer");
    }
    return Number(word >> 32n);
  }

  private pointerList(ptrWord: number): number[] {
    const lp = this.listPointer(ptrWord, 6);
    if (lp === null || lp.length === 0) {
      return [];
    }
    if (lp.length > MAX_TRAVERSAL_WORDS) {
      throw new Error("pointer list exceeds traversal budget");
    }
    this.checkRange(lp.target, lp.length);
    const out: number[] = [];
    for (let i = 0; i < lp.length; i++) {
      out.push(lp.target + i);
    }
    return out;
  }

  readTextList(ptrWord: number): string[] {
    return this.pointerList(ptrWord).map((w) => this.readText(w));
  }

  readDataList(ptrWord: number): Uint8Array[] {
    return this.pointerList(ptrWord).map((w) => this.readByteList(w));
  }

  readBoolList(ptrWord: number): boolean[] {
    const lp = this.listPointer(ptrWord, 1);
    if (lp === null || lp.length === 0) {
      return [];
    }
    this.checkRange(lp.target, Math.ceil(lp.length / 64));
    const base = this.segOff + lp.target * WORD;
    const out: boolean[] = [];
    for (let i = 0; i < lp.length; i++) {
      out.push(((this.view.getUint8(base + (i >> 3)) >> (i & 7)) & 1) === 1);
    }
    return out;
  }

  readStructList(
    ptrWord: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader[] {
    void dataWords;
    void pointerWords;
    const lp = this.listPointer(ptrWord, 7);
    if (lp === null || lp.length === 0) {
      return [];
    }
    const tagWord = lp.target;
    this.checkRange(tagWord, 1);
    const tag = this.readWord(tagWord);
    const count = Number((tag >> 2n) & 0x3fffffffn);
    const dw = Number((tag >> 32n) & 0xffffn);
    const pw = Number((tag >> 48n) & 0xffffn);
    const elemWords = dw + pw;
    if (elemWords > 0 && count > Math.floor(lp.length / elemWords)) {
      throw new Error("composite list count exceeds payload");
    }
    if (count > MAX_TRAVERSAL_WORDS) {
      throw new Error("composite list count exceeds traversal budget");
    }
    this.checkRange(tagWord, 1 + count * elemWords);
    const out: StructReader[] = [];
    for (let i = 0; i < count; i++) {
      out.push(new StructReader(this, tagWord + 1 + i * elemWords, dw, pw));
    }
    return out;
  }
}

function signExtend30(n: number): number {
  const v = n & 0x3fffffff;
  return v & 0x20000000 ? v - 0x40000000 : v;
}

/**
 * Struct cursor on a {@link CapnpReader}.
 *
 * Data fields outside the actual data section and pointers outside the
 * actual pointer section decode as defaults (zero / empty / null), which is
 * how Cap'n Proto reads a struct written by an older schema.
 */
export class StructReader {
  constructor(
    readonly reader: CapnpReader,
    readonly word: number,
    readonly dataWords: number,
    readonly pointerWords: number,
  ) {}

  private hasData(fieldIndex: number, size: number): boolean {
    return (fieldIndex + 1) * size <= this.dataWords * WORD;
  }

  pointerWord(index: number): number | null {
    if (index >= this.pointerWords) {
      return null;
    }
    return this.word + this.dataWords + index;
  }

  getUint16(fieldIndex: number): number {
    return this.hasData(fieldIndex, 2) ? this.reader.getUint16(this.word, fieldIndex) : 0;
  }

  getInt32(fieldIndex: number): number {
    return this.hasData(fieldIndex, 4) ? this.reader.getInt32(this.word, fieldIndex) : 0;
  }

  getUint32(fieldIndex: number): number {
    return this.hasData(fieldIndex, 4) ? this.reader.getUint32(this.word, fieldIndex) : 0;
  }

  getInt64(fieldIndex: number): bigint {
    return this.hasData(fieldIndex, 8) ? this.reader.getInt64(this.word, fieldIndex) : 0n;
  }

  getUint64(fieldIndex: number): bigint {
    return this.hasData(fieldIndex, 8) ? this.reader.getUint64(this.word, fieldIndex) : 0n;
  }

  getFloat32(fieldIndex: number): number {
    return this.hasData(fieldIndex, 4) ? this.reader.getFloat32(this.word, fieldIndex) : 0;
  }

  getFloat64(fieldIndex: number): number {
    return this.hasData(fieldIndex, 8) ? this.reader.getFloat64(this.word, fieldIndex) : 0;
  }

  getBool(bitIndex: number): boolean {
    if (bitIndex >= this.dataWords * 64) {
      return false;
    }
    return this.reader.getBool(this.word, bitIndex);
  }

  getText(pointerIndex: number): string {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? "" : this.reader.readText(ptr);
  }

  getData(pointerIndex: number): Uint8Array {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? new Uint8Array(0) : this.reader.readByteList(ptr);
  }

  getCapIndex(pointerIndex: number): number | null {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? null : this.reader.readCapIndex(ptr);
  }

  getTextList(pointerIndex: number): string[] {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readTextList(ptr);
  }

  getDataList(pointerIndex: number): Uint8Array[] {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readDataList(ptr);
  }

  getBoolList(pointerIndex: number): boolean[] {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readBoolList(ptr);
  }

  getStructList(
    pointerIndex: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader[] {
    const ptr = this.pointerWord(pointerIndex);
    return ptr === null ? [] : this.reader.readStructList(ptr, dataWords, pointerWords);
  }

  getStruct(
    pointerIndex: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader {
    const ptr = this.pointerWord(pointerIndex);
    if (ptr === null) {
      return new StructReader(this.reader, 0, 0, 0);
    }
    return this.reader.structAt(ptr, dataWords, pointerWords);
  }
}
