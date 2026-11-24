/**
 * Digests the destination needs: SHA-256 for object checksums and rule
 * names, SHA-1 for the per-upload checksum B2 checks, and a streaming
 * SHA-256 over an upload's parts.
 *
 * One-shot digests use Web Crypto. The streaming digest uses workerd's
 * `crypto.DigestStream`, so a large upload is hashed as it is read rather
 * than held in memory.
 */

/** Lower-case hex digits. */
const HEX_DIGITS = "0123456789abcdef";

/** Shared encoder for text input. */
const ENCODER = new TextEncoder();

/**
 * Lower-case hex of `bytes`.
 *
 * @param bytes - Any bytes.
 * @returns Two hex digits per byte.
 */
export function hex(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) {
    out += HEX_DIGITS.charAt(byte >> 4) + HEX_DIGITS.charAt(byte & 15);
  }
  return out;
}

/**
 * Reads 64 hex digits (either case, surrounding spaces ignored) as a
 * SHA-256 digest.
 *
 * @param text - The hex text.
 * @returns The 32 digest bytes, or `null` when `text` is not one.
 */
export function parseSha256Hex(text: string): Uint8Array | null {
  const trimmed = text.trim();
  if (!/^[0-9a-fA-F]{64}$/.test(trimmed)) return null;
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    out[i] = Number.parseInt(trimmed.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/**
 * The input as a fresh `ArrayBuffer`-backed view Web Crypto accepts.
 *
 * @param input - Text (hashed as UTF-8) or bytes.
 * @returns The bytes.
 */
function bytesOf(input: string | Uint8Array): Uint8Array<ArrayBuffer> {
  if (typeof input === "string") return ENCODER.encode(input);
  const copy = new Uint8Array(input.length);
  copy.set(input);
  return copy;
}

/**
 * SHA-256 of `input`, as lower-case hex.
 *
 * @param input - Text (hashed as UTF-8) or bytes.
 * @returns 64 hex digits.
 */
export async function sha256Hex(input: string | Uint8Array): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-256", bytesOf(input))));
}

/**
 * SHA-1 of `bytes`, as lower-case hex (the checksum B2 asks with every
 * upload and part).
 *
 * @param bytes - The upload body.
 * @returns 40 hex digits.
 */
export async function sha1Hex(bytes: Uint8Array): Promise<string> {
  return hex(new Uint8Array(await crypto.subtle.digest("SHA-1", bytesOf(bytes))));
}

/** The part of workerd's `crypto.DigestStream` this module uses. */
interface DigestStreamLike extends WritableStream<Uint8Array> {
  /** Resolves with the digest once the stream is closed. */
  readonly digest: Promise<ArrayBuffer>;
}

/** Constructor of {@link DigestStreamLike}. */
type DigestStreamConstructor = new (algorithm: string) => DigestStreamLike;

/**
 * An incremental SHA-256 over bytes fed in order.
 *
 * @throws {Error} From the constructor when the runtime has no
 *   `crypto.DigestStream`.
 */
export class Sha256Stream {
  /** The digest stream's writer. */
  readonly #writer: WritableStreamDefaultWriter<Uint8Array>;
  /** The digest, once the stream closes. */
  readonly #digest: Promise<ArrayBuffer>;

  /** Opens a digest. */
  constructor() {
    const Digest = (crypto as unknown as { DigestStream?: DigestStreamConstructor }).DigestStream;
    if (Digest === undefined) throw new Error("crypto.DigestStream is not available");
    const stream = new Digest("SHA-256");
    this.#writer = stream.getWriter();
    this.#digest = stream.digest;
  }

  /**
   * Feeds the next bytes.
   *
   * @param bytes - The bytes, in order.
   * @returns Resolves once they are taken.
   */
  async update(bytes: Uint8Array): Promise<void> {
    if (bytes.length > 0) await this.#writer.write(bytes);
  }

  /**
   * Ends the input.
   *
   * @returns The digest, as lower-case hex.
   */
  async finish(): Promise<string> {
    await this.#writer.close();
    return hex(new Uint8Array(await this.#digest));
  }
}
