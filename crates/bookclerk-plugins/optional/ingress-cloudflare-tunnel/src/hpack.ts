/**
 * HPACK (RFC 7541), sans-IO: the decoder for the header blocks the
 * Cloudflare edge sends, and an encoder for the connector's answers.
 *
 * The decoder keeps the dynamic table the edge's encoder fills, so every
 * header block must pass through it in order, including blocks for streams
 * the connector refuses. Strings are decoded byte for byte (Latin-1), as
 * HTTP field values are octets. The encoder writes each field as a literal
 * without indexing and without Huffman coding, which every decoder reads
 * and which leaves the edge's table alone.
 */

/** The static table (RFC 7541 appendix A); index 1 is entry 0. */
export const STATIC_TABLE: ReadonlyArray<readonly [string, string]> = [
  [":authority", ""],
  [":method", "GET"],
  [":method", "POST"],
  [":path", "/"],
  [":path", "/index.html"],
  [":scheme", "http"],
  [":scheme", "https"],
  [":status", "200"],
  [":status", "204"],
  [":status", "206"],
  [":status", "304"],
  [":status", "400"],
  [":status", "404"],
  [":status", "500"],
  ["accept-charset", ""],
  ["accept-encoding", "gzip, deflate"],
  ["accept-language", ""],
  ["accept-ranges", ""],
  ["accept", ""],
  ["access-control-allow-origin", ""],
  ["age", ""],
  ["allow", ""],
  ["authorization", ""],
  ["cache-control", ""],
  ["content-disposition", ""],
  ["content-encoding", ""],
  ["content-language", ""],
  ["content-length", ""],
  ["content-location", ""],
  ["content-range", ""],
  ["content-type", ""],
  ["cookie", ""],
  ["date", ""],
  ["etag", ""],
  ["expect", ""],
  ["expires", ""],
  ["from", ""],
  ["host", ""],
  ["if-match", ""],
  ["if-modified-since", ""],
  ["if-none-match", ""],
  ["if-range", ""],
  ["if-unmodified-since", ""],
  ["last-modified", ""],
  ["link", ""],
  ["location", ""],
  ["max-forwards", ""],
  ["proxy-authenticate", ""],
  ["proxy-authorization", ""],
  ["range", ""],
  ["referer", ""],
  ["refresh", ""],
  ["retry-after", ""],
  ["server", ""],
  ["set-cookie", ""],
  ["strict-transport-security", ""],
  ["transfer-encoding", ""],
  ["user-agent", ""],
  ["vary", ""],
  ["via", ""],
  ["www-authenticate", ""],
];

/** Huffman codes by symbol (RFC 7541 appendix B), right-aligned. */
export const HUFFMAN_CODES: readonly number[] = [
  0x1ff8, 0x7fffd8, 0xfffffe2, 0xfffffe3, 0xfffffe4, 0xfffffe5, 0xfffffe6, 0xfffffe7, 0xfffffe8,
  0xffffea, 0x3ffffffc, 0xfffffe9, 0xfffffea, 0x3ffffffd, 0xfffffeb, 0xfffffec, 0xfffffed,
  0xfffffee, 0xfffffef, 0xffffff0, 0xffffff1, 0xffffff2, 0x3ffffffe, 0xffffff3, 0xffffff4,
  0xffffff5, 0xffffff6, 0xffffff7, 0xffffff8, 0xffffff9, 0xffffffa, 0xffffffb, 0x14, 0x3f8, 0x3f9,
  0xffa, 0x1ff9, 0x15, 0xf8, 0x7fa, 0x3fa, 0x3fb, 0xf9, 0x7fb, 0xfa, 0x16, 0x17, 0x18, 0x0, 0x1,
  0x2, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x5c, 0xfb, 0x7ffc, 0x20, 0xffb, 0x3fc, 0x1ffa,
  0x21, 0x5d, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b,
  0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0xfc, 0x73, 0xfd, 0x1ffb, 0x7fff0, 0x1ffc, 0x3ffc, 0x22,
  0x7ffd, 0x3, 0x23, 0x4, 0x24, 0x5, 0x25, 0x26, 0x27, 0x6, 0x74, 0x75, 0x28, 0x29, 0x2a, 0x7, 0x2b,
  0x76, 0x2c, 0x8, 0x9, 0x2d, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7ffe, 0x7fc, 0x3ffd, 0x1ffd,
  0xffffffc, 0xfffe6, 0x3fffd2, 0xfffe7, 0xfffe8, 0x3fffd3, 0x3fffd4, 0x3fffd5, 0x7fffd9, 0x3fffd6,
  0x7fffda, 0x7fffdb, 0x7fffdc, 0x7fffdd, 0x7fffde, 0xffffeb, 0x7fffdf, 0xffffec, 0xffffed,
  0x3fffd7, 0x7fffe0, 0xffffee, 0x7fffe1, 0x7fffe2, 0x7fffe3, 0x7fffe4, 0x1fffdc, 0x3fffd8,
  0x7fffe5, 0x3fffd9, 0x7fffe6, 0x7fffe7, 0xffffef, 0x3fffda, 0x1fffdd, 0xfffe9, 0x3fffdb, 0x3fffdc,
  0x7fffe8, 0x7fffe9, 0x1fffde, 0x7fffea, 0x3fffdd, 0x3fffde, 0xfffff0, 0x1fffdf, 0x3fffdf,
  0x7fffeb, 0x7fffec, 0x1fffe0, 0x1fffe1, 0x3fffe0, 0x1fffe2, 0x7fffed, 0x3fffe1, 0x7fffee,
  0x7fffef, 0xfffea, 0x3fffe2, 0x3fffe3, 0x3fffe4, 0x7ffff0, 0x3fffe5, 0x3fffe6, 0x7ffff1,
  0x3ffffe0, 0x3ffffe1, 0xfffeb, 0x7fff1, 0x3fffe7, 0x7ffff2, 0x3fffe8, 0x1ffffec, 0x3ffffe2,
  0x3ffffe3, 0x3ffffe4, 0x7ffffde, 0x7ffffdf, 0x3ffffe5, 0xfffff1, 0x1ffffed, 0x7fff2, 0x1fffe3,
  0x3ffffe6, 0x7ffffe0, 0x7ffffe1, 0x3ffffe7, 0x7ffffe2, 0xfffff2, 0x1fffe4, 0x1fffe5, 0x3ffffe8,
  0x3ffffe9, 0xffffffd, 0x7ffffe3, 0x7ffffe4, 0x7ffffe5, 0xfffec, 0xfffff3, 0xfffed, 0x1fffe6,
  0x3fffe9, 0x1fffe7, 0x1fffe8, 0x7ffff3, 0x3fffea, 0x3fffeb, 0x1ffffee, 0x1ffffef, 0xfffff4,
  0xfffff5, 0x3ffffea, 0x7ffff4, 0x3ffffeb, 0x7ffffe6, 0x3ffffec, 0x3ffffed, 0x7ffffe7, 0x7ffffe8,
  0x7ffffe9, 0x7ffffea, 0x7ffffeb, 0xffffffe, 0x7ffffec, 0x7ffffed, 0x7ffffee, 0x7ffffef, 0x7fffff0,
  0x3ffffee,
];

/** Huffman code lengths by symbol, in bits. */
export const HUFFMAN_LENGTHS: readonly number[] = [
  13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 30, 28,
  28, 28, 28, 28, 28, 28, 28, 28, 6, 10, 10, 12, 13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6, 5, 5, 5,
  6, 6, 6, 6, 6, 6, 6, 7, 8, 15, 6, 12, 10, 13, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7,
  7, 7, 7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6, 15, 5, 6, 5, 6, 5, 6, 6, 6, 5, 7, 7, 6, 6, 6, 5, 6,
  7, 6, 5, 5, 6, 7, 7, 7, 7, 7, 15, 11, 14, 13, 28, 20, 22, 20, 20, 22, 22, 22, 23, 22, 23, 23, 23,
  23, 23, 24, 23, 24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23, 22, 23, 23, 24, 22, 21, 20, 22,
  22, 23, 23, 21, 23, 22, 22, 24, 21, 22, 23, 23, 21, 21, 22, 21, 23, 22, 23, 23, 20, 22, 22, 22,
  23, 22, 22, 23, 26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27, 27, 26, 24, 25, 19, 21, 26, 27,
  27, 26, 27, 24, 21, 21, 26, 26, 28, 27, 27, 27, 20, 24, 20, 21, 22, 21, 21, 23, 22, 22, 25, 25,
  24, 24, 26, 23, 26, 27, 26, 26, 27, 27, 27, 27, 27, 28, 27, 27, 27, 27, 27, 26,
];

/** The Huffman code that ends a string early, which a string must not hold. */
const HUFFMAN_EOS = { code: 0x3fffffff, length: 30 };

/** Default dynamic table size, in bytes (`SETTINGS_HEADER_TABLE_SIZE`). */
export const DEFAULT_TABLE_SIZE = 4096;

/** Overhead each dynamic table entry counts beyond its name and value. */
const ENTRY_OVERHEAD = 32;

/** Largest integer the decoder reads: past it the input is hostile. */
const MAX_INTEGER = 2 ** 28;

/** A header block the decoder refused: a `COMPRESSION_ERROR`. */
export class HpackError extends Error {
  /**
   * Creates an error.
   *
   * @param message - What was wrong.
   */
  constructor(message: string) {
    super(message);
    this.name = "HpackError";
  }
}

/**
 * The Huffman decoding tree: node `n` has children `tree[2n]` (bit 0) and
 * `tree[2n + 1]` (bit 1); a child at or past `LEAF` is the leaf for symbol
 * `child - LEAF` (256 is EOS), and 0 is a missing child.
 */
const LEAF = 1024;

/** The tree, built once. */
const TREE: Uint16Array = buildTree();

/**
 * Builds the Huffman decoding tree from the code table.
 *
 * @returns The tree.
 */
function buildTree(): Uint16Array {
  const tree = new Uint16Array(2 * LEAF);
  let nodes = 1;
  const add = (symbol: number, code: number, length: number): void => {
    let node = 0;
    for (let bit = length - 1; bit >= 0; bit--) {
      const slot = 2 * node + ((code >>> bit) & 1);
      if (bit === 0) {
        tree[slot] = LEAF + symbol;
      } else {
        if (tree[slot] === 0) tree[slot] = nodes++;
        node = tree[slot] as number;
      }
    }
  };
  for (let symbol = 0; symbol < 256; symbol++) {
    add(symbol, HUFFMAN_CODES[symbol] as number, HUFFMAN_LENGTHS[symbol] as number);
  }
  add(256, HUFFMAN_EOS.code, HUFFMAN_EOS.length);
  return tree;
}

/**
 * Decodes a Huffman-coded string (RFC 7541 section 5.2).
 *
 * @param bytes - The coded bytes.
 * @returns The octets.
 * @throws {HpackError} For an EOS symbol, padding longer than 7 bits or
 *   padding that is not the start of EOS (all ones).
 */
export function huffmanDecode(bytes: Uint8Array): Uint8Array {
  const out: number[] = [];
  let node = 0;
  // Bits read since the last whole symbol, and whether all were ones.
  let pending = 0;
  let ones = true;
  for (const byte of bytes) {
    for (let bit = 7; bit >= 0; bit--) {
      const b = (byte >>> bit) & 1;
      const next = TREE[2 * node + b] as number;
      pending++;
      ones &&= b === 1;
      if (next >= LEAF) {
        const symbol = next - LEAF;
        if (symbol === 256) throw new HpackError("a Huffman string holds EOS");
        out.push(symbol);
        node = 0;
        pending = 0;
        ones = true;
      } else {
        node = next;
      }
    }
  }
  if (pending > 7 || !ones) throw new HpackError("bad Huffman padding");
  return Uint8Array.from(out);
}

/**
 * Octets as a Latin-1 string, one character per byte.
 *
 * @param bytes - The octets.
 * @returns The string.
 */
function latin1(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 8192) {
    out += String.fromCharCode(...bytes.subarray(i, i + 8192));
  }
  return out;
}

/** A field the decoder read. */
export type Field = [name: string, value: string];

/** What one header block held. */
export interface DecodedBlock {
  /** The fields, in order, up to the list limit. */
  fields: Field[];
  /** Whether the list passed the limit (the fields are then incomplete). */
  oversized: boolean;
}

/**
 * The decoding side of one HPACK context: the edge's encoder's dynamic
 * table, kept in step block by block.
 */
export class HpackDecoder {
  /** Dynamic table entries, newest first. */
  readonly #table: Field[] = [];
  /** Bytes the table holds, by RFC 7541's count. */
  #size = 0;
  /** The table size the encoder chose, at most {@link HpackDecoder.#limit}. */
  #max: number;
  /** The table size this side allowed (its `SETTINGS_HEADER_TABLE_SIZE`). */
  readonly #limit: number;
  /** Largest field list a block may decode to, by RFC 9113's count. */
  readonly #maxList: number;

  /**
   * Creates a decoder.
   *
   * @param maxListBytes - Largest decoded field list (name and value bytes
   *   plus 32 per field); a longer one decodes with `oversized` set.
   * @param tableSize - The table size this side advertises.
   */
  constructor(maxListBytes: number, tableSize = DEFAULT_TABLE_SIZE) {
    this.#maxList = maxListBytes;
    this.#limit = tableSize;
    this.#max = tableSize;
  }

  /** Bytes the dynamic table holds, for tests. */
  get tableBytes(): number {
    return this.#size;
  }

  /**
   * Decodes one complete header block.
   *
   * @param block - The block (HEADERS and CONTINUATION fragments joined).
   * @returns The fields.
   * @throws {HpackError} For any coding error; the context is then broken
   *   and the connection must end.
   */
  decode(block: Uint8Array): DecodedBlock {
    const fields: Field[] = [];
    let listBytes = 0;
    let oversized = false;
    let at = 0;
    let fieldSeen = false;
    const integer = (prefix: number): number => {
      const mask = (1 << prefix) - 1;
      if (at >= block.length) throw new HpackError("truncated integer");
      let value = (block[at++] as number) & mask;
      if (value < mask) return value;
      let shift = 0;
      for (;;) {
        if (at >= block.length) throw new HpackError("truncated integer");
        const byte = block[at++] as number;
        value += (byte & 0x7f) * 2 ** shift;
        shift += 7;
        if (value > MAX_INTEGER) throw new HpackError("integer too large");
        if ((byte & 0x80) === 0) return value;
      }
    };
    const string = (): string => {
      if (at >= block.length) throw new HpackError("truncated string");
      const huffman = ((block[at] as number) & 0x80) !== 0;
      const length = integer(7);
      if (length > block.length - at) throw new HpackError("string past the block");
      const raw = block.subarray(at, at + length);
      at += length;
      return latin1(huffman ? huffmanDecode(raw) : raw);
    };
    const entry = (index: number): Field => {
      if (index === 0) throw new HpackError("index 0");
      if (index <= STATIC_TABLE.length) {
        const [name, value] = STATIC_TABLE[index - 1] as readonly [string, string];
        return [name, value];
      }
      const field = this.#table[index - STATIC_TABLE.length - 1];
      if (field === undefined) throw new HpackError(`index ${index} past the table`);
      return [field[0], field[1]];
    };
    const emit = (field: Field): void => {
      fieldSeen = true;
      listBytes += field[0].length + field[1].length + ENTRY_OVERHEAD;
      if (listBytes > this.#maxList) oversized = true;
      if (!oversized) fields.push(field);
    };
    while (at < block.length) {
      const first = block[at] as number;
      if (first & 0x80) {
        emit(entry(integer(7)));
      } else if (first & 0x40) {
        const index = integer(6);
        const name = index === 0 ? string() : entry(index)[0];
        const field: Field = [name, string()];
        this.#insert(field);
        emit(field);
      } else if (first & 0x20) {
        if (fieldSeen) throw new HpackError("table size update after a field");
        const size = integer(5);
        if (size > this.#limit) throw new HpackError("table size past the setting");
        this.#max = size;
        this.#evict(0);
      } else {
        // Literal without indexing (0000) or never indexed (0001).
        const index = integer(4);
        const name = index === 0 ? string() : entry(index)[0];
        emit([name, string()]);
      }
    }
    return { fields, oversized };
  }

  /**
   * Adds an entry, evicting the oldest ones to make room.
   *
   * @param field - The entry.
   */
  #insert(field: Field): void {
    const size = field[0].length + field[1].length + ENTRY_OVERHEAD;
    this.#evict(size);
    if (size > this.#max) return; // Larger than the table: it empties it.
    this.#table.unshift(field);
    this.#size += size;
  }

  /**
   * Evicts the oldest entries until `room` more bytes fit.
   *
   * @param room - Bytes to make room for.
   */
  #evict(room: number): void {
    while (this.#table.length > 0 && this.#size + room > this.#max) {
      const old = this.#table.pop() as Field;
      this.#size -= old[0].length + old[1].length + ENTRY_OVERHEAD;
    }
  }
}

/**
 * Writes an HPACK integer.
 *
 * @param out - Where the bytes go.
 * @param value - The integer.
 * @param prefix - Prefix bits.
 * @param flags - The first byte's high bits.
 */
function writeInteger(out: number[], value: number, prefix: number, flags: number): void {
  const mask = (1 << prefix) - 1;
  if (value < mask) {
    out.push(flags | value);
    return;
  }
  out.push(flags | mask);
  let rest = value - mask;
  while (rest >= 0x80) {
    out.push((rest % 0x80) | 0x80);
    rest = Math.floor(rest / 0x80);
  }
  out.push(rest);
}

/**
 * Writes a raw (not Huffman-coded) string of Latin-1 octets.
 *
 * @param out - Where the bytes go.
 * @param text - The string; characters past U+00FF are written as `?`.
 */
function writeString(out: number[], text: string): void {
  writeInteger(out, text.length, 7, 0);
  for (let i = 0; i < text.length; i++) {
    const code = text.charCodeAt(i);
    out.push(code <= 0xff ? code : 0x3f);
  }
}

/**
 * Encodes fields as one header block: each a literal without indexing,
 * with a literal name, so the edge's dynamic table never changes.
 *
 * @param fields - Lowercase names and their values.
 * @returns The block.
 */
export function encodeHeaders(fields: ReadonlyArray<readonly [string, string]>): Uint8Array {
  const out: number[] = [];
  for (const [name, value] of fields) {
    out.push(0);
    writeString(out, name);
    writeString(out, value);
  }
  return Uint8Array.from(out);
}
