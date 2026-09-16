/**
 * Unpacked Cap'n Proto helpers for the typed SQL ABI.
 *
 * Not a public SDK export. Layouts match `schema/plugin.capnp` as compiled
 * by `capnpc-rust` (`write_message` single-segment stream).
 */

const WORD = 8;

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
      (BigInt(offset) << 2n) |
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
      (BigInt(offset) << 2n) |
      (BigInt(elementSize) << 32n) |
      (BigInt(listLength) << 35n);
    this.setWord(ptrWord, word);
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
    const encoded = new TextEncoder().encode(value);
    const withNul = new Uint8Array(encoded.length + 1);
    withNul.set(encoded, 0);
    this.setByteList(ptrWord, withNul);
  }

  setData(ptrWord: number, value: Uint8Array): void {
    this.setByteList(ptrWord, value);
  }

  setByteList(ptrWord: number, bytes: Uint8Array): void {
    const nWords = Math.ceil(bytes.length / WORD) || (bytes.length === 0 ? 0 : 1);
    if (bytes.length === 0) {
      this.writeListPointer(ptrWord, ptrWord + 1, 2, 0);
      return;
    }
    const target = this.alloc(nWords);
    this.buf.set(bytes, target * WORD);
    this.writeListPointer(ptrWord, target, 2, bytes.length);
  }

  setUint16(word: number, fieldIndex: number, value: number): void {
    this.view.setUint16(word * WORD + fieldIndex * 2, value, true);
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

  setFloat64(word: number, fieldIndex: number, value: number): void {
    this.view.setFloat64(word * WORD + fieldIndex * 8, value, true);
  }

  setBool(word: number, bitIndex: number, value: boolean): void {
    const byteOff = word * WORD + (bitIndex >> 3);
    const mask = 1 << (bitIndex & 7);
    if (value) {
      this.buf[byteOff] |= mask;
    } else {
      this.buf[byteOff] &= ~mask;
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
    return this.word + this.dataWords + index;
  }

  setUint16(fieldIndex: number, value: number): void {
    this.msg.setUint16(this.word, fieldIndex, value);
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

export class CapnpReader {
  private readonly view: DataView;
  private readonly segOff: number;
  private readonly size0: number;
  private static readonly MAX_TRAVERSAL_WORDS = 64 * 1024;

  constructor(bytes: Uint8Array) {
    if (bytes.byteLength < WORD) {
      throw new Error("truncated Cap'n message");
    }
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const nsegMinus = this.view.getUint32(0, true);
    const nseg = nsegMinus + 1;
    if (nseg !== 1) {
      throw new Error("multi-segment Cap'n messages are not supported");
    }
    const size0 = this.view.getUint32(4, true);
    this.segOff = WORD;
    this.size0 = size0;
    if (this.segOff + size0 * WORD > bytes.byteLength) {
      throw new Error("truncated Cap'n segment");
    }
    if (size0 > CapnpReader.MAX_TRAVERSAL_WORDS) {
      throw new Error("Cap'n segment exceeds traversal budget");
    }
  }

  root(dataWords: number, pointerWords: number): StructReader {
    return this.structAt(0, dataWords, pointerWords);
  }

  structAt(ptrWord: number, dataWords: number, pointerWords: number): StructReader {
    const word = this.readWord(ptrWord);
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
    if (dw < dataWords || pw < pointerWords) {
      throw new Error("struct pointer smaller than expected");
    }
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

  getUint16(word: number, fieldIndex: number): number {
    return this.view.getUint16(this.segOff + word * WORD + fieldIndex * 2, true);
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

  getFloat64(word: number, fieldIndex: number): number {
    return this.view.getFloat64(this.segOff + word * WORD + fieldIndex * 8, true);
  }

  getBool(word: number, bitIndex: number): boolean {
    const byteOff = this.segOff + word * WORD + (bitIndex >> 3);
    return (this.view.getUint8(byteOff) & (1 << (bitIndex & 7))) !== 0;
  }

  readByteList(ptrWord: number): Uint8Array {
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return new Uint8Array(0);
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
    if (c !== 2) {
      throw new Error("expected byte list");
    }
    const target = ptrWord + 1 + offset;
    const nWords = d === 0 ? 0 : Math.ceil(d / WORD);
    this.checkRange(target, nWords);
    const start = this.segOff + target * WORD;
    if (start + d > this.view.byteLength) {
      throw new Error("truncated Cap'n byte list");
    }
    return new Uint8Array(this.view.buffer, this.view.byteOffset + start, d);
  }

  readText(ptrWord: number): string {
    const bytes = this.readByteList(ptrWord);
    const end = bytes.length > 0 && bytes[bytes.length - 1] === 0 ? bytes.length - 1 : bytes.length;
    return new TextDecoder().decode(bytes.subarray(0, end));
  }

  readStructList(
    ptrWord: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader[] {
    const word = this.readWord(ptrWord);
    if (word === 0n) {
      return [];
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
    if (c !== 7) {
      throw new Error("expected composite list");
    }
    if (d === 0) {
      return [];
    }
    const tagWord = ptrWord + 1 + offset;
    this.checkRange(tagWord, 1);
    const tag = this.readWord(tagWord);
    const count = Number((tag >> 2n) & 0x3fffffffn);
    const dw = Number((tag >> 32n) & 0xffffn);
    const pw = Number((tag >> 48n) & 0xffffn);
    if (dw < dataWords || pw < pointerWords) {
      throw new Error("composite element smaller than expected");
    }
    const elemWords = dw + pw;
    if (elemWords > 0 && count > Math.floor(d / elemWords)) {
      throw new Error("composite list count exceeds payload");
    }
    if (count > CapnpReader.MAX_TRAVERSAL_WORDS) {
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

export class StructReader {
  constructor(
    readonly reader: CapnpReader,
    readonly word: number,
    readonly dataWords: number,
    readonly pointerWords: number,
  ) {}

  pointerWord(index: number): number {
    return this.word + this.dataWords + index;
  }

  getUint16(fieldIndex: number): number {
    return this.reader.getUint16(this.word, fieldIndex);
  }

  getUint32(fieldIndex: number): number {
    return this.reader.getUint32(this.word, fieldIndex);
  }

  getInt64(fieldIndex: number): bigint {
    return this.reader.getInt64(this.word, fieldIndex);
  }

  getUint64(fieldIndex: number): bigint {
    return this.reader.getUint64(this.word, fieldIndex);
  }

  getFloat64(fieldIndex: number): number {
    return this.reader.getFloat64(this.word, fieldIndex);
  }

  getBool(bitIndex: number): boolean {
    return this.reader.getBool(this.word, bitIndex);
  }

  getText(pointerIndex: number): string {
    return this.reader.readText(this.pointerWord(pointerIndex));
  }

  getData(pointerIndex: number): Uint8Array {
    return this.reader.readByteList(this.pointerWord(pointerIndex));
  }

  getStructList(
    pointerIndex: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader[] {
    return this.reader.readStructList(
      this.pointerWord(pointerIndex),
      dataWords,
      pointerWords,
    );
  }

  getStruct(
    pointerIndex: number,
    dataWords: number,
    pointerWords: number,
  ): StructReader {
    return this.reader.structAt(this.pointerWord(pointerIndex), dataWords, pointerWords);
  }
}
