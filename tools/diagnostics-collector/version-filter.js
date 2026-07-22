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

/** @param {string | null | undefined} raw */
export function normalizeVersion(raw) {
  if (raw == null) return "";
  let s = String(raw).trim();
  if (!s) return "";
  if ((s[0] === "v" || s[0] === "V") && /^\d/.test(s.slice(1))) {
    s = s.slice(1);
  }
  return s;
}

/**
 * Object keys look like `diagnostics/<version>/<report_id>.json`.
 * @param {string} fileName
 * @returns {string | null}
 */
export function extractVersionFromKey(fileName) {
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
 * @param {string} candidate
 * @param {string} baseline
 */
export function isPackagingDerivative(candidate, baseline) {
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
 * @returns {{ major: number, minor: number, patch: number, prerelease: string | null } | null}
 */
export function parseSemver(raw) {
  const s = normalizeVersion(raw);
  if (!s) return null;
  const core = s.split("+", 1)[0];
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
 * @returns {-1 | 0 | 1 | null} null if either side is not parseable
 */
export function compareSemver(a, b) {
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
  if (pa.prerelease === pb.prerelease) return 0;
  return pa.prerelease < pb.prerelease ? -1 : 1;
}

/**
 * @param {string | null | undefined} candidate object version (or null if unknown)
 * @param {string | null | undefined} baseline latest stable; null/empty = accept all
 */
export function versionAcceptable(candidate, baseline) {
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
