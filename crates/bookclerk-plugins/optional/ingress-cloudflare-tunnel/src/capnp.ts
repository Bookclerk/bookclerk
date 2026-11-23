/**
 * The part of Cap'n Proto's encoding the tunnel's control channel needs,
 * sans-IO: the stream framing, a bounded reader for messages from the edge
 * and a single-segment builder for the connector's.
 *
 * The reader follows the encoding spec (capnproto.org/encoding.html):
 * struct, list and far pointers, with a traversal limit and a nesting limit
 * so a hostile message can't make it loop or read without end. Reads past
 * a struct's sections return the field's zero value, as a newer schema's
 * fields do for an older reader. Capability pointers read as `null`.
 */

/** Most segments one message may have. */
export const MAX_SEGMENTS = 64;

/** Largest message the reader takes, in bytes of segment data. */
export const MAX_MESSAGE_BYTES = 1024 * 1024;

/** Most words a message's pointers may lead the reader through. */
const TRAVERSAL_LIMIT_WORDS = MAX_MESSAGE_BYTES / 8;

/** Deepest pointer nesting the reader follows. */
const NESTING_LIMIT = 64;

/** A message the reader refused. */
export class CapnpError extends Error {
  /**
   * Creates an error.
   *
   * @param message - What was wrong.
   */
  constructor(message: string) {
    super(message);
    this.name = "CapnpError";
  }
}

/**
 * Splits a byte stream into framed messages (the standard stream framing:
 * a segment table, padding to a word, then the segments).
 */
export class MessageFramer {
  /** Bytes not yet parsed. */
  #buf: Uint8Array = new Uint8Array();

  /** Bytes held, for the session's memory cap. */
  get buffered(): number {
    return this.#buf.length;
  }

  /**
   * Reads bytes.
   *
   * @param chunk - The next bytes.
   * @returns The messages they complete, each as its segments.
   * @throws {CapnpError} For a segment table past the limits.
   */
  push(chunk: Uint8Array): Uint8Array[][] {
    if (chunk.length > 0) {
      const joined = new Uint8Array(this.#buf.length + chunk.length);
      joined.set(this.#buf);
      joined.set(chunk, this.#buf.length);
      this.#buf = joined;
    }
    const out: Uint8Array[][] = [];
    for (;;) {
      if (this.#buf.length < 4) return out;
      const view = new DataView(this.#buf.buffer, this.#buf.byteOffset, this.#buf.byteLength);
      const count = view.getUint32(0, true) + 1;
      if (count > MAX_SEGMENTS) throw new CapnpError(`${count} segments`);
      const table = 4 + 4 * count + (count % 2 === 0 ? 4 : 0);
      if (this.#buf.length < table) return out;
      const sizes: number[] = [];
      let total = 0;
      for (let i = 0; i < count; i++) {
        const words = view.getUint32(4 + 4 * i, true);
        sizes.push(words * 8);
        total += words * 8;
        if (total > MAX_MESSAGE_BYTES) throw new CapnpError("message too large");
      }
      if (this.#buf.length < table + total) return out;
      const segments: Uint8Array[] = [];
      let at = table;
      for (const size of sizes) {
        segments.push(this.#buf.slice(at, at + size));
        at += size;
      }
      out.push(segments);
      this.#buf = this.#buf.slice(at);
    }
  }
}

/** Shared state of one message being read. */
class Reading {
  /** The segments, as views. */
  readonly segments: DataView[];
  /** Words the reader may still traverse. */
  budget = TRAVERSAL_LIMIT_WORDS;

  /**
   * Wraps a message's segments.
   *
   * @param segments - The segments.
   */
  constructor(segments: Uint8Array[]) {
    this.segments = segments.map((s) => new DataView(s.buffer, s.byteOffset, s.byteLength));
  }

  /**
   * Charges words against the traversal limit.
   *
   * @param words - Words read.
   * @throws {CapnpError} When the limit is spent.
   */
  charge(words: number): void {
    this.budget -= Math.max(words, 1);
    if (this.budget < 0) throw new CapnpError("traversal limit spent");
  }
}

/** Where a pointer leads, after far pointers. */
interface Target {
  /** The segment. */
  seg: number;
  /** The content's first word in the segment. */
  word: number;
  /** The pointer's low 32 bits (kind and, for struct/list, the offset). */
  lo: number;
  /** The pointer's high 32 bits (sizes). */
  hi: number;
}

/**
 * Reads the pointer at a word and resolves far pointers.
 *
 * @param msg - The message.
 * @param seg - Segment of the pointer.
 * @param word - Word of the pointer.
 * @returns The target, or `null` for a null pointer.
 * @throws {CapnpError} For a pointer out of bounds.
 */
function resolve(msg: Reading, seg: number, word: number): Target | null {
  const view = msg.segments[seg] as DataView;
  if ((word + 1) * 8 > view.byteLength) throw new CapnpError("pointer past its segment");
  let lo = view.getUint32(word * 8, true);
  let hi = view.getUint32(word * 8 + 4, true);
  if (lo === 0 && hi === 0) return null;
  if ((lo & 3) !== 2) {
    // Offset: a signed 30-bit count of words from the pointer's end.
    return { seg, word: word + 1 + (lo >> 2), lo, hi };
  }
  // A far pointer: a landing pad in another segment.
  const double = (lo & 4) !== 0;
  const padWord = lo >>> 3;
  const padSeg = hi;
  const pad = msg.segments[padSeg];
  if (pad === undefined) throw new CapnpError("far pointer to no segment");
  if ((padWord + (double ? 2 : 1)) * 8 > pad.byteLength) {
    throw new CapnpError("landing pad past its segment");
  }
  lo = pad.getUint32(padWord * 8, true);
  hi = pad.getUint32(padWord * 8 + 4, true);
  if (!double) {
    if ((lo & 3) === 2) throw new CapnpError("far pointer to a far pointer");
    return { seg: padSeg, word: padWord + 1 + (lo >> 2), lo, hi };
  }
  // Double: a far pointer to the content, then a tag with offset 0.
  if ((lo & 7) !== 2) throw new CapnpError("bad double-far landing pad");
  const contentWord = lo >>> 3;
  const contentSeg = hi;
  if (msg.segments[contentSeg] === undefined) throw new CapnpError("far pointer to no segment");
  const tagLo = pad.getUint32(padWord * 8 + 8, true);
  const tagHi = pad.getUint32(padWord * 8 + 12, true);
  if ((tagLo & 3) === 2) throw new CapnpError("double-far tag is a far pointer");
  return { seg: contentSeg, word: contentWord, lo: tagLo & 3, hi: tagHi };
}

/**
 * Checks that words lie inside a segment.
 *
 * @param msg - The message.
 * @param seg - The segment.
 * @param word - First word.
 * @param words - Word count.
 * @throws {CapnpError} When they don't.
 */
function bounds(msg: Reading, seg: number, word: number, words: number): void {
  const view = msg.segments[seg] as DataView;
  if (word < 0 || (word + words) * 8 > view.byteLength) {
    throw new CapnpError("content past its segment");
  }
}

/** A struct being read. */
export class StructReader {
  /** The message. */
  readonly #msg: Reading;
  /** Its segment. */
  readonly #seg: number;
  /** First word of the data section. */
  readonly #data: number;
  /** Data section words. */
  readonly #dataWords: number;
  /** Pointer count. */
  readonly #ptrs: number;
  /** Nesting depth. */
  readonly #depth: number;

  /**
   * Creates a reader (use {@link readRoot}).
   *
   * @param msg - The message.
   * @param seg - Segment.
   * @param data - First data word.
   * @param dataWords - Data words.
   * @param ptrs - Pointer count.
   * @param depth - Nesting depth.
   */
  constructor(
    msg: Reading,
    seg: number,
    data: number,
    dataWords: number,
    ptrs: number,
    depth: number,
  ) {
    this.#msg = msg;
    this.#seg = seg;
    this.#data = data;
    this.#dataWords = dataWords;
    this.#ptrs = ptrs;
    this.#depth = depth;
  }

  /** The segment view. */
  get #view(): DataView {
    return this.#msg.segments[this.#seg] as DataView;
  }

  /**
   * An unsigned integer in the data section.
   *
   * @param byte - Byte offset in the section.
   * @param size - Width in bytes (1, 2 or 4).
   * @returns The value, or 0 past the section.
   */
  uint(byte: number, size: 1 | 2 | 4): number {
    if (byte + size > this.#dataWords * 8) return 0;
    const at = this.#data * 8 + byte;
    if (size === 1) return this.#view.getUint8(at);
    if (size === 2) return this.#view.getUint16(at, true);
    return this.#view.getUint32(at, true);
  }

  /**
   * A signed 64-bit integer in the data section.
   *
   * @param byte - Byte offset in the section.
   * @returns The value, or 0 past the section.
   */
  int64(byte: number): bigint {
    if (byte + 8 > this.#dataWords * 8) return 0n;
    return this.#view.getBigInt64(this.#data * 8 + byte, true);
  }

  /**
   * A boolean in the data section.
   *
   * @param bit - Bit offset in the section.
   * @returns The stored bit (XOR with the field's default is the caller's).
   */
  bool(bit: number): boolean {
    return (this.uint(bit >>> 3, 1) & (1 << (bit & 7))) !== 0;
  }

  /**
   * Resolves pointer `i`.
   *
   * @param i - Pointer index.
   * @returns The target, or `null` when null, a capability or past the
   *   section.
   */
  #pointer(i: number): Target | null {
    if (i >= this.#ptrs) return null;
    if (this.#depth >= NESTING_LIMIT) throw new CapnpError("nesting limit");
    const target = resolve(this.#msg, this.#seg, this.#data + this.#dataWords + i);
    // A capability pointer: the connector holds no capabilities.
    return target !== null && (target.lo & 3) === 3 ? null : target;
  }

  /**
   * The struct at pointer `i`.
   *
   * @param i - Pointer index.
   * @returns The struct, or `null`.
   * @throws {CapnpError} When the pointer is not a struct pointer or is out
   *   of bounds.
   */
  struct(i: number): StructReader | null {
    const target = this.#pointer(i);
    if (target === null) return null;
    return structAt(this.#msg, target, this.#depth + 1);
  }

  /**
   * The byte list at pointer `i`.
   *
   * @param i - Pointer index.
   * @returns The bytes (a copy), empty for a null pointer.
   * @throws {CapnpError} When the pointer is not a byte list or is out of
   *   bounds.
   */
  data(i: number): Uint8Array {
    const target = this.#pointer(i);
    if (target === null) return new Uint8Array();
    if ((target.lo & 3) !== 1 || (target.hi & 7) !== 2) {
      throw new CapnpError("expected a byte list");
    }
    const count = target.hi >>> 3;
    const words = Math.ceil(count / 8);
    bounds(this.#msg, target.seg, target.word, words);
    this.#msg.charge(words);
    const view = this.#msg.segments[target.seg] as DataView;
    return new Uint8Array(
      view.buffer.slice(
        view.byteOffset + target.word * 8,
        view.byteOffset + target.word * 8 + count,
      ),
    );
  }

  /**
   * The text at pointer `i`.
   *
   * @param i - Pointer index.
   * @returns The text, empty for a null pointer; invalid UTF-8 is replaced.
   * @throws {CapnpError} When it is not a NUL-terminated byte list.
   */
  text(i: number): string {
    const bytes = this.data(i);
    if (bytes.length === 0) return "";
    if (bytes[bytes.length - 1] !== 0) throw new CapnpError("text without its NUL");
    return new TextDecoder().decode(bytes.subarray(0, bytes.length - 1));
  }
}

/**
 * The struct a resolved pointer leads to.
 *
 * @param msg - The message.
 * @param target - The target.
 * @param depth - Nesting depth.
 * @returns The struct.
 * @throws {CapnpError} When it is not a struct pointer or is out of bounds.
 */
function structAt(msg: Reading, target: Target, depth: number): StructReader {
  if ((target.lo & 3) !== 0) throw new CapnpError("expected a struct pointer");
  const dataWords = target.hi & 0xffff;
  const ptrs = target.hi >>> 16;
  bounds(msg, target.seg, target.word, dataWords + ptrs);
  msg.charge(dataWords + ptrs);
  return new StructReader(msg, target.seg, target.word, dataWords, ptrs, depth);
}

/**
 * Reads a message's root struct.
 *
 * @param segments - The message's segments.
 * @returns The root.
 * @throws {CapnpError} When the root is missing or not a struct.
 */
export function readRoot(segments: Uint8Array[]): StructReader {
  if (segments.length === 0) throw new CapnpError("no segments");
  const msg = new Reading(segments);
  const target = resolve(msg, 0, 0);
  if (target === null) throw new CapnpError("null root");
  return structAt(msg, target, 0);
}

/** A single-segment message being built. */
export class Builder {
  /** The segment so far. */
  #bytes = new Uint8Array(256);
  /** Words in use. */
  #words = 1; // Word 0 is the root pointer.

  /**
   * Allocates zeroed words.
   *
   * @param words - Word count.
   * @returns The first word's index.
   */
  alloc(words: number): number {
    const at = this.#words;
    this.#words += words;
    if (this.#words * 8 > this.#bytes.length) {
      const grown = new Uint8Array(Math.max(this.#bytes.length * 2, this.#words * 8));
      grown.set(this.#bytes);
      this.#bytes = grown;
    }
    return at;
  }

  /** A view of the segment (valid until the next allocation). */
  get view(): DataView {
    return new DataView(this.#bytes.buffer);
  }

  /** The segment's bytes. */
  get bytes(): Uint8Array {
    return this.#bytes;
  }

  /**
   * Writes a struct or list pointer.
   *
   * @param at - The pointer's word.
   * @param target - The content's first word.
   * @param kind - 0 struct, 1 list.
   * @param hi - The high 32 bits.
   */
  pointer(at: number, target: number, kind: 0 | 1, hi: number): void {
    const view = this.view;
    view.setInt32(at * 8, ((target - at - 1) << 2) | kind, true);
    view.setUint32(at * 8 + 4, hi, true);
  }

  /**
   * Creates the root struct.
   *
   * @param dataWords - Data section words.
   * @param ptrs - Pointer count.
   * @returns The struct.
   */
  root(dataWords: number, ptrs: number): StructBuilder {
    return StructBuilder.at(this, 0, dataWords, ptrs);
  }

  /**
   * The message with its stream framing.
   *
   * @returns The bytes.
   */
  frame(): Uint8Array {
    const out = new Uint8Array(8 + this.#words * 8);
    const view = new DataView(out.buffer);
    view.setUint32(0, 0, true);
    view.setUint32(4, this.#words, true);
    out.set(this.#bytes.subarray(0, this.#words * 8), 8);
    return out;
  }

  /**
   * The segment alone, for tests.
   *
   * @returns The bytes.
   */
  segment(): Uint8Array {
    return this.#bytes.slice(0, this.#words * 8);
  }
}

/** A struct being built. */
export class StructBuilder {
  /** The message. */
  readonly #b: Builder;
  /** First data word. */
  readonly #data: number;
  /** Data words. */
  readonly #dataWords: number;
  /** Pointer count. */
  readonly #ptrs: number;

  /**
   * Wraps allocated words.
   *
   * @param b - The message.
   * @param data - First data word.
   * @param dataWords - Data words.
   * @param ptrs - Pointer count.
   */
  constructor(b: Builder, data: number, dataWords: number, ptrs: number) {
    this.#b = b;
    this.#data = data;
    this.#dataWords = dataWords;
    this.#ptrs = ptrs;
  }

  /**
   * Allocates a struct and points the pointer at `at` to it.
   *
   * @param b - The message.
   * @param at - The pointer's word.
   * @param dataWords - Data words.
   * @param ptrs - Pointer count.
   * @returns The struct.
   */
  static at(b: Builder, at: number, dataWords: number, ptrs: number): StructBuilder {
    const word = b.alloc(dataWords + ptrs);
    b.pointer(at, word, 0, dataWords | (ptrs << 16));
    return new StructBuilder(b, word, dataWords, ptrs);
  }

  /**
   * The word of pointer `i`.
   *
   * @param i - Pointer index.
   * @returns The word.
   */
  #ptr(i: number): number {
    if (i >= this.#ptrs) throw new RangeError(`pointer ${i} of ${this.#ptrs}`);
    return this.#data + this.#dataWords + i;
  }

  /**
   * Sets an unsigned integer.
   *
   * @param byte - Byte offset in the data section.
   * @param size - Width in bytes.
   * @param value - The value.
   * @returns This struct.
   */
  uint(byte: number, size: 1 | 2 | 4, value: number): this {
    const at = this.#data * 8 + byte;
    const view = this.#b.view;
    if (size === 1) view.setUint8(at, value);
    else if (size === 2) view.setUint16(at, value, true);
    else view.setUint32(at, value, true);
    return this;
  }

  /**
   * Sets an unsigned 64-bit integer.
   *
   * @param byte - Byte offset in the data section.
   * @param value - The value.
   * @returns This struct.
   */
  uint64(byte: number, value: bigint): this {
    this.#b.view.setBigUint64(this.#data * 8 + byte, value, true);
    return this;
  }

  /**
   * Sets a boolean's stored bit.
   *
   * @param bit - Bit offset in the data section.
   * @param value - The bit.
   * @returns This struct.
   */
  bool(bit: number, value: boolean): this {
    const at = this.#data * 8 + (bit >>> 3);
    const view = this.#b.view;
    const old = view.getUint8(at);
    view.setUint8(at, value ? old | (1 << (bit & 7)) : old & ~(1 << (bit & 7)));
    return this;
  }

  /**
   * Creates a struct at pointer `i`.
   *
   * @param i - Pointer index.
   * @param dataWords - Data words.
   * @param ptrs - Pointer count.
   * @returns The new struct.
   */
  struct(i: number, dataWords: number, ptrs: number): StructBuilder {
    return StructBuilder.at(this.#b, this.#ptr(i), dataWords, ptrs);
  }

  /**
   * Sets a byte list at pointer `i`.
   *
   * @param i - Pointer index.
   * @param bytes - The bytes.
   * @returns This struct.
   */
  data(i: number, bytes: Uint8Array): this {
    writeBytes(this.#b, this.#ptr(i), bytes);
    return this;
  }

  /**
   * Sets text at pointer `i`.
   *
   * @param i - Pointer index.
   * @param text - The text.
   * @returns This struct.
   */
  text(i: number, text: string): this {
    const utf8 = new TextEncoder().encode(text);
    const bytes = new Uint8Array(utf8.length + 1);
    bytes.set(utf8);
    writeBytes(this.#b, this.#ptr(i), bytes);
    return this;
  }

  /**
   * Sets a list of texts at pointer `i`.
   *
   * @param i - Pointer index.
   * @param texts - The texts.
   * @returns This struct.
   */
  textList(i: number, texts: readonly string[]): this {
    const list = this.#b.alloc(texts.length);
    this.#b.pointer(this.#ptr(i), list, 1, 6 | (texts.length << 3));
    texts.forEach((text, n) => {
      const utf8 = new TextEncoder().encode(text);
      const bytes = new Uint8Array(utf8.length + 1);
      bytes.set(utf8);
      writeBytes(this.#b, list + n, bytes);
    });
    return this;
  }
}

/**
 * Writes a byte list and points the pointer at `at` to it.
 *
 * @param b - The message.
 * @param at - The pointer's word.
 * @param bytes - The bytes.
 */
function writeBytes(b: Builder, at: number, bytes: Uint8Array): void {
  const word = b.alloc(Math.ceil(bytes.length / 8));
  b.bytes.set(bytes, word * 8);
  b.pointer(at, word, 1, 2 | (bytes.length << 3));
}
