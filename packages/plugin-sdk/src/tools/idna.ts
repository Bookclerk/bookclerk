/**
 * IDNA A-label checks shared by the manifest validator and the sparse
 * workerd materializer.
 *
 * Some URL parsers pass an `xn--` label through without decoding it; the
 * host's IDNA step requires each to decode to the label it encodes.
 */

/**
 * True unless `label` is an `xn--` label that does not decode, or whose
 * decoded form does not encode back to it.
 *
 * @param label - One lowercase ASCII label.
 * @returns Whether the label is acceptable.
 */
export function isValidALabel(label: string): boolean {
  if (!label.startsWith("xn--")) return true;
  const decoded = decodePunycode(label.slice(4));
  if (decoded === null || decoded.length === 0) return false;
  try {
    return new URL(`http://${decoded}.example/`).hostname === `${label}.example`;
  } catch {
    return false;
  }
}

/**
 * Decodes a Punycode string (RFC 3492, without the `xn--` prefix).
 *
 * @param input - The encoded label body, lowercase ASCII.
 * @returns The decoded label, or `null` when the input is not valid Punycode.
 */
function decodePunycode(input: string): string | null {
  const base = 36;
  const tMin = 1;
  const tMax = 26;
  const skew = 38;
  const damp = 700;
  const adapt = (delta: number, points: number, first: boolean): number => {
    let d = first ? Math.floor(delta / damp) : delta >> 1;
    d += Math.floor(d / points);
    let k = 0;
    while (d > ((base - tMin) * tMax) >> 1) {
      d = Math.floor(d / (base - tMin));
      k += base;
    }
    return k + Math.floor(((base - tMin + 1) * d) / (d + skew));
  };
  const digit = (code: number): number => {
    if (code >= 0x30 && code <= 0x39) return code - 22;
    if (code >= 0x61 && code <= 0x7a) return code - 0x61;
    return -1;
  };
  const delimiter = input.lastIndexOf("-");
  const output: number[] = [];
  for (let i = 0; i < Math.max(delimiter, 0); i++) {
    const code = input.charCodeAt(i);
    if (code >= 0x80) return null;
    output.push(code);
  }
  let n = 0x80;
  let i = 0;
  let bias = 72;
  for (let pos = delimiter > 0 ? delimiter + 1 : 0; pos < input.length;) {
    const oldI = i;
    let w = 1;
    for (let k = base; ; k += base) {
      if (pos >= input.length) return null;
      const d = digit(input.charCodeAt(pos++));
      if (d < 0) return null;
      i += d * w;
      if (i > 0x10ffff * (output.length + 1)) return null;
      const t = k <= bias ? tMin : k >= bias + tMax ? tMax : k - bias;
      if (d < t) break;
      w *= base - t;
    }
    bias = adapt(i - oldI, output.length + 1, oldI === 0);
    n += Math.floor(i / (output.length + 1));
    i %= output.length + 1;
    if (n > 0x10ffff) return null;
    output.splice(i, 0, n);
    i++;
  }
  try {
    return String.fromCodePoint(...output);
  } catch {
    return null;
  }
}
