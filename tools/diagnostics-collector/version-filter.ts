/**
 * Version acceptance for diagnostics /report selection.
 *
 * Given a baseline (latest *stable* GitHub release tag), keep object versions that are:
 * - equal to the baseline
 * - newer (semver greater — covers prereleases of a future release)
 * - derivative (third-party repack appending to the baseline, e.g. 1.2.3+nix, 1.2.3-1)
 *
 * No baseline (no GitHub releases yet) → accept all.
 */

/**
 * A version without surrounding space or a leading `v` before a digit.
 *
 * @param raw - Version text, or null / undefined for none.
 * @returns The normalized version, "" for none.
 */
export function normalizeVersion(raw: string | null | undefined): string {
  if (raw == null) return "";
  let s = String(raw).trim();
  if (!s) return "";
  if ((s[0] === "v" || s[0] === "V") && /^\d/.test(s.slice(1))) {
    s = s.slice(1);
  }
  return s;
}

/**
 * The version segment of an object key. Object keys look like
 * `diagnostics/<version>/<report_id>.json`.
 *
 * @param fileName - Object key.
 * @returns The normalized version, or null when the key has none.
 */
export function extractVersionFromKey(fileName: string): string | null {
  const parts = String(fileName || "")
    .split("/")
    .filter(Boolean);
  // ["diagnostics", "<version>", "<uuid>.json"]
  if (parts.length < 3 || parts[0] !== "diagnostics") return null;
  const version = parts[1];
  if (!version || version === "unknown" || version === "report") return null;
  return normalizeVersion(version);
}

/**
 * Distro / rebuild suffixes on the exact baseline (not semver prereleases like -rc.1).
 *
 * @param candidate - Version to classify.
 * @param baseline - Release it may repackage.
 * @returns True when `candidate` is `baseline` or `baseline` plus a
 *   packaging suffix.
 */
export function isPackagingDerivative(candidate: string, baseline: string): boolean {
  const c = normalizeVersion(candidate);
  const b = normalizeVersion(baseline);
  if (!c || !b) return false;
  if (c === b) return true;
  if (!c.toLowerCase().startsWith(b.toLowerCase())) return false;
  const rest = c.slice(b.length);
  if (rest.startsWith("+")) return true; // +build
  if (/^\.\w/.test(rest)) return true; // .fc40 / .1
  if (/^-\d/.test(rest)) return true; // Debian revision 1.2.3-1ubuntu2
  if (rest.startsWith("_")) return true;
  return false;
}

/**
 * Minimal semver parse: major.minor.patch + optional -prerelease (ignore +build).
 *
 * @param raw - Version text.
 * @returns The parts, or null when it does not parse.
 */
export function parseSemver(
  raw: string | null | undefined,
): { major: number; minor: number; patch: number; prerelease: string | null } | null {
  const s = normalizeVersion(raw);
  if (!s) return null;
  const core = s.split("+", 1)[0] ?? "";
  const m = core.match(/^(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?$/);
  if (!m) return null;
  return {
    major: Number(m[1]),
    minor: Number(m[2] || 0),
    patch: Number(m[3] || 0),
    prerelease: m[4] || null,
  };
}

/**
 * Orders two versions by semver precedence.
 *
 * @param a - One version.
 * @param b - Another.
 * @returns -1, 0 or 1, or null if either side is not parseable.
 */
export function compareSemver(
  a: string | null | undefined,
  b: string | null | undefined,
): -1 | 0 | 1 | null {
  const pa = parseSemver(a);
  const pb = parseSemver(b);
  if (!pa || !pb) return null;
  if (pa.major !== pb.major) return pa.major < pb.major ? -1 : 1;
  if (pa.minor !== pb.minor) return pa.minor < pb.minor ? -1 : 1;
  if (pa.patch !== pb.patch) return pa.patch < pb.patch ? -1 : 1;
  // Semver: release without prerelease > same version with prerelease.
  if (pa.prerelease == null && pb.prerelease == null) return 0;
  if (pa.prerelease == null) return 1;
  if (pb.prerelease == null) return -1;
  return comparePrerelease(pa.prerelease, pb.prerelease);
}

/**
 * Compare two prerelease strings the way semver 2.0 item 11 orders them:
 * dot-separated identifiers compared left to right, numeric identifiers
 * numerically, numeric before alphanumeric, alphanumeric by ASCII, and a
 * shorter list of identifiers before a longer one that shares its prefix.
 *
 * @param a - prerelease part of the first version (without the `-`)
 * @param b - prerelease part of the second version
 * @returns ordering of `a` relative to `b`
 */
function comparePrerelease(a: string, b: string): -1 | 0 | 1 {
  const ia = a.split(".");
  const ib = b.split(".");
  const n = Math.min(ia.length, ib.length);
  for (let i = 0; i < n; i++) {
    const x = ia[i] ?? "";
    const y = ib[i] ?? "";
    if (x === y) continue;
    const xn = /^\d+$/.test(x);
    const yn = /^\d+$/.test(y);
    if (xn && yn) return BigInt(x) < BigInt(y) ? -1 : 1;
    if (xn) return -1;
    if (yn) return 1;
    return x < y ? -1 : 1;
  }
  if (ia.length === ib.length) return 0;
  return ia.length < ib.length ? -1 : 1;
}

/**
 * Whether `/report` returns an object of this version.
 *
 * @param candidate - object version (or null if unknown)
 * @param baseline - latest stable; null/empty = accept all
 * @returns True for the baseline, a repackaging of it, or a later version.
 */
export function versionAcceptable(
  candidate: string | null | undefined,
  baseline: string | null | undefined,
): boolean {
  const b = normalizeVersion(baseline);
  if (!b) return true;
  const c = normalizeVersion(candidate);
  if (!c) return false;
  if (c.toLowerCase() === b.toLowerCase()) return true;
  if (isPackagingDerivative(c, b)) return true;
  const cmp = compareSemver(c, b);
  if (cmp == null) return false;
  return cmp > 0;
}
