/**
 * Addresses and names, parsed without I/O so the fuzz targets and the tests
 * drive the same code the plugin runs.
 *
 * - {@link parseEchoAnswer} reads what the address echo answered: one IPv4
 *   or IPv6 address and an optional newline. It returns the address in the
 *   form Cloudflare stores it, so a record that already points there is
 *   left alone.
 * - {@link isPublicAddress} keeps private, shared, documentation and other
 *   special-purpose ranges out of public DNS.
 * - {@link parseHostname} and {@link zoneCandidates} read the host name
 *   setting.
 */

/** An address family: 4 for IPv4, 6 for IPv6. */
export type Family = 4 | 6;

/** Longest echo answer read, in bytes; an IPv6 address is at most 45 characters. */
export const MAX_ECHO_BYTES = 64;

/** Longest host name, in bytes (RFC 1035). */
export const MAX_HOSTNAME_BYTES = 253;

/** One DNS label: lowercase letters, digits and inner dashes, 1 to 63 long. */
const LABEL = /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;

/** One decimal octet of an IPv4 address, without leading zeros. */
const OCTET = /^(?:0|[1-9][0-9]{0,2})$/;

/** One IPv6 group: 1 to 4 hex digits. */
const GROUP = /^[0-9a-fA-F]{1,4}$/;

/**
 * Parses dotted-quad IPv4 text.
 *
 * @param text - Candidate address.
 * @returns The four octets, or `null` unless it is exactly four decimal
 *   octets of 0 to 255 without leading zeros.
 */
export function parseIpv4(text: string): number[] | null {
  const parts = text.split(".");
  if (parts.length !== 4) return null;
  const octets: number[] = [];
  for (const part of parts) {
    if (!OCTET.test(part)) return null;
    const value = Number(part);
    if (value > 255) return null;
    octets.push(value);
  }
  return octets;
}

/**
 * Parses IPv6 text (RFC 4291 section 2.2), with `::` and a trailing
 * dotted-quad allowed and no zone.
 *
 * @param text - Candidate address.
 * @returns The eight 16-bit groups, or `null` when it is not an address.
 */
export function parseIpv6(text: string): number[] | null {
  const halves = text.split("::");
  if (halves.length > 2) return null;
  const compressed = halves.length === 2;
  const sides = halves.map((half) => (half === "" ? [] : half.split(":")));
  const groups: number[][] = [[], []];
  for (let side = 0; side < sides.length; side++) {
    const parts = sides[side] as string[];
    for (let i = 0; i < parts.length; i++) {
      const part = parts[i] as string;
      const last = side === sides.length - 1 && i === parts.length - 1;
      if (last && part.includes(".")) {
        const octets = parseIpv4(part);
        if (octets === null) return null;
        const [a, b, c, d] = octets as [number, number, number, number];
        (groups[side] as number[]).push((a << 8) | b, (c << 8) | d);
      } else if (GROUP.test(part)) {
        (groups[side] as number[]).push(parseInt(part, 16));
      } else {
        return null;
      }
    }
  }
  const [head, tail] = groups as [number[], number[]];
  const explicit = head.length + tail.length;
  if (compressed) {
    if (explicit > 7) return null;
    return [...head, ...new Array<number>(8 - explicit).fill(0), ...tail];
  }
  return explicit === 8 ? head : null;
}

/**
 * Formats IPv6 groups as RFC 5952 recommends: lowercase hex without
 * leading zeros, and the longest run of two or more zero groups (the first
 * of equal runs) as `::`.
 *
 * @param groups - Eight 16-bit groups.
 * @returns The text.
 */
export function formatIpv6(groups: readonly number[]): string {
  let bestStart = -1;
  let bestLength = 1;
  for (let i = 0; i < 8;) {
    if (groups[i] !== 0) {
      i++;
      continue;
    }
    let j = i;
    while (j < 8 && groups[j] === 0) j++;
    if (j - i > bestLength) {
      bestStart = i;
      bestLength = j - i;
    }
    i = j;
  }
  const hex = groups.map((group) => group.toString(16));
  if (bestStart < 0) return hex.join(":");
  const head = hex.slice(0, bestStart).join(":");
  const tail = hex.slice(bestStart + bestLength).join(":");
  return `${head}::${tail}`;
}

/**
 * Reads the address echo's answer.
 *
 * @param body - The answer's text, at most {@link MAX_ECHO_BYTES}.
 * @param family - The family asked for.
 * @returns The address in its canonical form (dotted quad, or RFC 5952
 *   text), or `null` unless the body is one address of `family` and at
 *   most one trailing newline.
 */
export function parseEchoAnswer(body: string, family: Family): string | null {
  const text = body.endsWith("\n") ? body.slice(0, -1) : body;
  if (family === 4) return parseIpv4(text) === null ? null : text;
  const groups = parseIpv6(text);
  return groups === null ? null : formatIpv6(groups);
}

/**
 * Whether an IPv4 address is in a range special-purpose registries keep out
 * of the public internet (RFC 6890 and its updates).
 *
 * @param octets - The address.
 * @returns `true` for private, shared (CGNAT), loopback, link-local,
 *   documentation, benchmarking, multicast and reserved addresses.
 */
function specialIpv4(octets: readonly number[]): boolean {
  const [a, b, c] = octets as [number, number, number, number];
  return (
    a === 0 ||
    a === 10 ||
    a === 127 ||
    a >= 224 ||
    (a === 100 && b >= 64 && b < 128) ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b < 32) ||
    (a === 192 && b === 0 && (c === 0 || c === 2)) ||
    (a === 192 && b === 88 && c === 99) ||
    (a === 192 && b === 168) ||
    (a === 198 && (b === 18 || b === 19)) ||
    (a === 198 && b === 51 && c === 100) ||
    (a === 203 && b === 0 && c === 113)
  );
}

/**
 * Whether an IPv6 address can be reached from the internet: global unicast
 * (`2000::/3`) outside the protocol (`2001::/23`), documentation
 * (`2001:db8::/32`, `3fff::/20`) and 6to4 (`2002::/16`) ranges.
 *
 * @param groups - The address.
 * @returns `true` for a public address.
 */
function publicIpv6(groups: readonly number[]): boolean {
  const [a, b] = groups as [number, number];
  if ((a & 0xe000) !== 0x2000) return false;
  if (a === 0x2001 && b < 0x200) return false;
  if (a === 0x2001 && b === 0xdb8) return false;
  if (a === 0x2002) return false;
  return !(a === 0x3fff && b < 0x1000);
}

/**
 * Whether a canonical address may be published.
 *
 * @param address - A dotted quad or IPv6 text.
 * @param family - Its family.
 * @returns `true` when it parses and is public.
 */
export function isPublicAddress(address: string, family: Family): boolean {
  if (family === 4) {
    const octets = parseIpv4(address);
    return octets !== null && !specialIpv4(octets);
  }
  const groups = parseIpv6(address);
  return groups !== null && publicIpv6(groups);
}

/**
 * Reads the host name setting.
 *
 * @param value - The setting, as the operator typed it.
 * @returns The lowercase name without a trailing dot, or `null` unless it
 *   is at least two DNS labels, at most {@link MAX_HOSTNAME_BYTES} bytes,
 *   with a top label that starts with a letter (so it is never an IP
 *   address) and no `xn--` label.
 */
export function parseHostname(value: string): string | null {
  let name = value.trim().toLowerCase();
  if (name.endsWith(".")) name = name.slice(0, -1);
  if (name.length > MAX_HOSTNAME_BYTES) return null;
  const labels = name.split(".");
  if (labels.length < 2) return null;
  for (const label of labels) {
    if (!LABEL.test(label) || label.startsWith("xn--")) return null;
  }
  return /^[a-z]/.test(labels[labels.length - 1] as string) ? name : null;
}

/**
 * The zones a host name may be in, longest first: the name itself, then
 * each parent with at least two labels.
 *
 * @param hostname - A name {@link parseHostname} accepted.
 * @returns The candidates.
 */
export function zoneCandidates(hostname: string): string[] {
  const labels = hostname.split(".");
  const out: string[] = [];
  for (let i = 0; i + 2 <= labels.length; i++) out.push(labels.slice(i).join("."));
  return out;
}
