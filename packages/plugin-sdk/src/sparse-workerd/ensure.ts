/**
 * Downloads / refreshes the pinned Cloudflare `workerd` binary (mirrors `ensure.rs`).
 */

import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import { gunzipSync } from "node:zlib";
import path from "node:path";
import { fileURLToPath } from "node:url";
import os from "node:os";

/**
 * One platform asset entry inside {@link WorkerdPin}.
 */
export type WorkerdAsset = {
  /** Gzip-compressed release artifact filename on the Cloudflare workerd GitHub release. */
  artifact: string;
  /** Expected SHA-256 hex digest of the compressed artifact bytes (not the gunzipped binary). */
  sha256_hex: string;
};

/**
 * Contents of `workerd-pin.json` shipping with this package.
 */
export type WorkerdPin = {
  /** GitHub release tag (for example `v1.20250310.0`) that identifies this pin. */
  release_tag: string;
  /** Compatibility date bundled with this pin for Cap'n Proto configs. */
  bundled_compat_date: string;
  /** Stamp filename written beside the installed binary to record `release_tag`. */
  version_stamp: string;
  /** Platform key → artifact map (`linux-x86_64`, `macos-aarch64`, `windows-x86_64`, …). */
  assets: Record<string, WorkerdAsset>;
};

/**
 * Resolves the `@bookclerk/plugin-sdk` package root.
 *
 * Works whether this module is loaded from `src/sparse-workerd` or
 * `dist/sparse-workerd`.
 *
 * @returns Absolute path to `packages/plugin-sdk`.
 */
export function packageRoot(): string {
  const here = path.dirname(fileURLToPath(import.meta.url));
  // dist/sparse-workerd or src/sparse-workerd → package root
  return path.resolve(here, "../..");
}

/**
 * Loads and parses `workerd-pin.json` from the package root.
 *
 * @param root - Package root override (defaults to {@link packageRoot}).
 * @returns Parsed pin document.
 */
export function loadPin(root = packageRoot()): WorkerdPin {
  const pinPath = assertPathInside(path.resolve(root), "workerd-pin.json");
  return JSON.parse(fs.readFileSync(pinPath, "utf8")) as WorkerdPin;
}

/**
 * Maps Node `process.platform` / `process.arch` to a pin asset key.
 *
 * @param platform - Node platform string (default `process.platform`).
 * @param arch - Node arch string (default `process.arch`).
 * @returns Pin asset key, or `undefined` when the host is unsupported.
 */
export function platformKey(
  platform = process.platform,
  arch = process.arch,
): string | undefined {
  const map: Record<string, string> = {
    "linux-x64": "linux-x86_64",
    "linux-arm64": "linux-aarch64",
    "darwin-x64": "macos-x86_64",
    "darwin-arm64": "macos-aarch64",
    "win32-x64": "windows-x86_64",
  };
  return map[`${platform}-${arch}`];
}

/**
 * Returns the on-disk workerd binary basename for the current platform.
 *
 * @returns `workerd.exe` on Windows, otherwise `workerd`.
 */
export function binaryName(): string {
  return process.platform === "win32" ? "workerd.exe" : "workerd";
}

/**
 * Builds the GitHub release download URL for a pin artifact.
 *
 * @param pin - Loaded workerd pin.
 * @param artifact - Artifact filename from {@link WorkerdAsset.artifact}.
 * @returns Absolute HTTPS download URL.
 */
export function downloadUrl(pin: WorkerdPin, artifact: string): string {
  return `https://github.com/cloudflare/workerd/releases/download/${pin.release_tag}/${artifact}`;
}

/**
 * Resolve the workerd cache directory.
 *
 * Honors `BOOKCLERK_WORKERD_CACHE` as the selected install root (workspace
 * `target/`, `/opt`, external volumes). Otherwise uses
 * `~/.cache/bookclerk/workerd`. Derived files stay under that root.
 *
 * @returns Absolute cache directory path.
 */
export function defaultCacheDir(): string {
  const raw = process.env.BOOKCLERK_WORKERD_CACHE;
  if (raw && !raw.includes("\0")) {
    return path.resolve(raw);
  }
  return path.join(path.resolve(os.homedir()), ".cache", "bookclerk", "workerd");
}

/**
 * Resolves `candidate` under `root` via `path.resolve` + `path.relative` barrier.
 *
 * Rejects NUL bytes and `..` path components (names like `..draft` and
 * `edition..2` are allowed). Uses the CodeQL-recognized
 * `path.relative` / `startsWith("..")` containment pattern — no filesystem
 * probes before the barrier (those are sinks under local threat modeling).
 *
 * @param root - Trusted directory (resolved).
 * @param candidate - Absolute path or path relative to `root`.
 * @returns Absolute resolved path under `root`.
 * @throws {Error} When the path is empty, contains NUL/`..`, or escapes `root`.
 */
export function assertPathInside(root: string, candidate: string): string {
  if (!root || root.includes("\0") || !candidate || candidate.includes("\0")) {
    throw new Error("path is empty or contains NUL");
  }
  const normalized = candidate.replace(/\\/g, "/");
  if (normalized.split("/").includes("..")) {
    throw new Error(`path must not contain '..': ${candidate}`);
  }
  const resolvedRoot = path.resolve(root);
  const resolved = path.isAbsolute(candidate)
    ? path.resolve(candidate)
    : path.resolve(resolvedRoot, candidate);
  // CodeQL RelativePathStartsWithSanitizer / StartsWithDirSanitizer shape.
  const rel = path.relative(resolvedRoot, resolved);
  if (rel.startsWith(".." + path.sep) || rel === ".." || path.isAbsolute(rel)) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  if (!resolved.startsWith(resolvedRoot + path.sep) && resolved !== resolvedRoot) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  return resolved;
}

/**
 * Create `root` / `rel` as a directory after resolve + relative/`startsWith` barrier.
 *
 * @param root - Trusted directory.
 * @param rel - Relative suffix (may be multi-segment when each segment is safe).
 * @returns Absolute directory path under `root`.
 */
export function ensureDirUnder(root: string, rel: string): string {
  const resolvedRoot = path.resolve(root);
  const resolved = assertPathInside(resolvedRoot, rel);
  const relCheck = path.relative(resolvedRoot, resolved);
  if (
    relCheck.startsWith(".." + path.sep) ||
    relCheck === ".." ||
    path.isAbsolute(relCheck)
  ) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  // Inspect existing components before recursive mkdir so a symlink prefix
  // (e.g. `root/@bookclerk -> /outside`) cannot create `/outside/plugin-sdk`.
  refuseSymlinkExistingComponents(resolvedRoot, resolved);
  fs.mkdirSync(resolved, { recursive: true });
  return resolved;
}

/**
 * Write `contents` to `root` / `name` (single component) after resolve + barrier.
 *
 * Assumes `root` already exists (session/cache dirs are created first). No
 * mkdir of the tainted root — only write the barriered child path.
 *
 * @param root - Trusted directory (must already exist).
 * @param name - Single path component filename.
 * @param contents - Bytes or string to write.
 * @returns Absolute file path under `root`.
 */
export function writeFileUnder(
  root: string,
  name: string,
  contents: string | NodeJS.ArrayBufferView,
): string {
  singleFileComponent(name);
  const resolvedRoot = path.resolve(root);
  const resolved = path.resolve(resolvedRoot, name);
  const rel = path.relative(resolvedRoot, resolved);
  if (rel.startsWith(".." + path.sep) || rel === ".." || path.isAbsolute(rel)) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  if (!resolved.startsWith(resolvedRoot + path.sep) && resolved !== resolvedRoot) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  refuseSymlinkPath(resolvedRoot, resolved);
  fs.writeFileSync(resolved, contents);
  return resolved;
}

/**
 * Copy `src` to `root` / `name` after resolve + barrier on the destination.
 *
 * @param root - Trusted destination directory (must already exist).
 * @param name - Single path component filename.
 * @param src - Absolute source file (already validated by the caller).
 * @returns Absolute destination path under `root`.
 */
export function copyFileUnder(root: string, name: string, src: string): string {
  singleFileComponent(name);
  const resolvedRoot = path.resolve(root);
  const resolved = path.resolve(resolvedRoot, name);
  const rel = path.relative(resolvedRoot, resolved);
  if (rel.startsWith(".." + path.sep) || rel === ".." || path.isAbsolute(rel)) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  if (!resolved.startsWith(resolvedRoot + path.sep) && resolved !== resolvedRoot) {
    throw new Error(`path ${resolved} escapes root ${resolvedRoot}`);
  }
  refuseSymlinkPath(resolvedRoot, resolved);
  fs.copyFileSync(src, resolved);
  return resolved;
}

/**
 * Require `candidate` under `trustedRoot` with no symlink among *existing*
 * suffix components.
 *
 * Unlike {@link refuseSymlinkPath}, any trailing missing components are
 * allowed (not only the final one). Use before `mkdirSync(recursive)` so a
 * symlink intermediate cannot redirect directory creation outside the root.
 *
 * @param trustedRoot - Original operator/plugin root.
 * @param candidate - Path previously produced by {@link assertPathInside}.
 * @returns The validated absolute path.
 * @throws {Error} When an existing suffix component is a symlink or escapes.
 */
export function refuseSymlinkExistingComponents(
  trustedRoot: string,
  candidate: string,
): string {
  const rootLex = path.resolve(trustedRoot);
  const target = path.resolve(candidate);
  const rel = path.relative(rootLex, target);
  if (
    path.isAbsolute(rel) ||
    rel === ".." ||
    rel.startsWith(".." + path.sep) ||
    rel.split(path.sep).includes("..")
  ) {
    throw new Error(`path ${target} escapes root ${rootLex}`);
  }
  const root = fs.realpathSync(rootLex);
  const parts = rel === "" ? [] : rel.split(path.sep);
  let cur = root;
  for (let i = 0; i < parts.length; i++) {
    cur = path.resolve(cur, parts[i]!);
    const stepRel = path.relative(root, cur);
    if (
      path.isAbsolute(stepRel) ||
      stepRel === ".." ||
      stepRel.startsWith(".." + path.sep)
    ) {
      throw new Error(`path ${cur} escapes root ${root}`);
    }
    if (!cur.startsWith(root + path.sep) && cur !== root) {
      throw new Error(`path ${cur} escapes root ${root}`);
    }
    let st: fs.Stats;
    try {
      st = fs.lstatSync(cur);
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code === "ENOENT") {
        // Missing from here on: safe to create under the last existing prefix.
        break;
      }
      throw err;
    }
    if (st.isSymbolicLink()) {
      throw new Error(`refusing symlink in path: ${cur}`);
    }
  }
  return target;
}

/**
 * Require `candidate` under `trustedRoot` with no symlink suffix components.
 *
 * Resolves the trusted root identity once (a symlinked operator/plugin root is
 * allowed). Only path components *below* that identity are refused when they
 * are symlinks.
 *
 * @param trustedRoot - Original operator/plugin root.
 * @param candidate - Path previously produced by {@link assertPathInside}.
 * @returns The validated absolute path.
 * @throws {Error} When a suffix component is a symlink or escapes `trustedRoot`.
 */
export function refuseSymlinkPath(trustedRoot: string, candidate: string): string {
  const rootLex = path.resolve(trustedRoot);
  const target = path.resolve(candidate);
  const rel = path.relative(rootLex, target);
  if (
    path.isAbsolute(rel) ||
    rel === ".." ||
    rel.startsWith(".." + path.sep) ||
    rel.split(path.sep).includes("..")
  ) {
    throw new Error(`path ${target} escapes root ${rootLex}`);
  }
  // Allow the operator-selected root itself to be a symlink; constrain children.
  const root = fs.realpathSync(rootLex);
  const parts = rel === "" ? [] : rel.split(path.sep);
  let cur = root;
  for (let i = 0; i < parts.length; i++) {
    cur = path.resolve(cur, parts[i]!);
    const stepRel = path.relative(root, cur);
    if (
      path.isAbsolute(stepRel) ||
      stepRel === ".." ||
      stepRel.startsWith(".." + path.sep)
    ) {
      throw new Error(`path ${cur} escapes root ${root}`);
    }
    if (!cur.startsWith(root + path.sep) && cur !== root) {
      throw new Error(`path ${cur} escapes root ${root}`);
    }
    let st: fs.Stats;
    try {
      st = fs.lstatSync(cur);
    } catch (err) {
      const code = (err as NodeJS.ErrnoException).code;
      if (code === "ENOENT" && i === parts.length - 1) {
        break;
      }
      throw err;
    }
    if (st.isSymbolicLink()) {
      throw new Error(`refusing symlink in path: ${cur}`);
    }
  }
  return target;
}

/**
 * Validates a URL before `fetch` / download.
 *
 * Allows `https:` anywhere, or `http:` only to loopback hosts.
 *
 * @param url - Absolute URL string.
 * @returns Canonical href safe to request.
 * @throws {Error} When the URL is invalid or uses a disallowed scheme/host.
 */
export function validateFetchUrl(url: string): string {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    throw new Error(`invalid URL: ${url}`);
  }
  if (parsed.protocol === "https:") {
    return parsed.href;
  }
  if (parsed.protocol === "http:") {
    const host = parsed.hostname.toLowerCase();
    if (
      host === "127.0.0.1" ||
      host === "localhost" ||
      host === "::1" ||
      host === "[::1]"
    ) {
      return parsed.href;
    }
  }
  throw new Error(
    `refusing non-HTTPS (or non-loopback HTTP) URL: ${parsed.protocol}//${parsed.host}`,
  );
}

/**
 * Validates a workerd (or helper) binary path before spawn.
 *
 * Requires an absolute path with no NUL bytes. When `trustedRoot` is set,
 * resolves both paths and requires the binary to stay under that root.
 *
 * @param bin - Candidate executable path.
 * @param trustedRoot - Optional directory the binary must remain under.
 * @returns Absolute validated path.
 * @throws {Error} When the path is relative, contains NUL, or escapes `trustedRoot`.
 */
export function validateSpawnExecutable(
  bin: string,
  trustedRoot?: string,
): string {
  if (!bin || bin.includes("\0")) {
    throw new Error("spawn executable path is empty or contains NUL");
  }
  if (!path.isAbsolute(bin)) {
    throw new Error(`spawn executable must be absolute: ${bin}`);
  }
  if (trustedRoot) {
    return assertPathInside(trustedRoot, bin);
  }
  return path.resolve(bin);
}

function singleFileComponent(name: string): void {
  if (
    !name ||
    name.includes("\0") ||
    name.includes("/") ||
    name.includes("\\") ||
    name === "." ||
    name === ".."
  ) {
    throw new Error(`file name must be a single path component: ${name}`);
  }
}

function stampFileName(pin: WorkerdPin): string {
  const stamp = pin.version_stamp;
  if (
    !stamp ||
    stamp.includes("\0") ||
    stamp.includes("/") ||
    stamp.includes("\\") ||
    stamp === ".." ||
    stamp === "."
  ) {
    throw new Error(`invalid workerd version_stamp: ${stamp}`);
  }
  return stamp;
}

function usableFile(bin: string): boolean {
  try {
    // Follow leaf symlinks so BOOKCLERK_WORKERD_BIN overrides that point at a
    // pinned executable still match. Managed-cache callers refuse symlink
    // leaves before invoking binaryMatchesPin.
    return fs.statSync(bin).isFile();
  } catch {
    return false;
  }
}

/**
 * True when `bin` is a usable file matching `pin`.
 *
 * A sibling version stamp is enough when the binary exists. A stamp without a
 * binary is not current. When no Bookclerk stamp matches, probe `--version`
 * on the absolute path with fixed argv (`shell: false`, no `PATH` lookup).
 *
 * @param bin - Absolute candidate executable.
 * @param pin - Loaded workerd pin.
 * @returns Whether the binary is present and matches the pin.
 */
export function binaryMatchesPin(bin: string, pin: WorkerdPin): boolean {
  if (!usableFile(bin)) return false;
  try {
    const dir = path.resolve(path.dirname(bin));
    const stamp = assertPathInside(dir, stampFileName(pin));
    const stampSt = fs.lstatSync(stamp);
    if (
      !stampSt.isSymbolicLink() &&
      stampSt.isFile() &&
      fs.readFileSync(stamp, "utf8").trim() === pin.release_tag
    ) {
      return true;
    }
  } catch {
    // Missing or invalid stamp → probe the absolute binary.
  }
  const validated = validateSpawnExecutable(bin);
  const out = spawnSync(validated, ["--version"], { encoding: "utf8", shell: false });
  if (out.status !== 0) return false;
  const combined = `${out.stdout ?? ""}${out.stderr ?? ""}`;
  const pinBare = pin.release_tag.replace(/^v/, "");
  return combined.includes(pin.release_tag) || combined.includes(pinBare);
}

/**
 * Ensures `cacheDir/workerd` matches the pin, downloading if needed.
 *
 * Honors `BOOKCLERK_WORKERD_BIN` when that absolute binary exists and matches
 * the pin (stamp or `--version`). The selected cache directory is the trusted
 * install root; children stay under it.
 *
 * @param cacheDir - Directory that will hold the binary (default {@link defaultCacheDir}).
 * @param root - Package root for loading the pin (default {@link packageRoot}).
 * @returns Absolute path to a current `workerd` binary.
 * @throws {Error} When the platform has no pin, download fails, or the digest mismatches.
 */
export async function ensureWorkerd(
  cacheDir = defaultCacheDir(),
  root = packageRoot(),
): Promise<string> {
  const pin = loadPin(root);
  const override = process.env.BOOKCLERK_WORKERD_BIN;
  if (override && !override.includes("\0") && path.isAbsolute(override)) {
    try {
      if (binaryMatchesPin(override, pin)) {
        return validateSpawnExecutable(override);
      }
    } catch {
      // Unusable override → fall through to cache install.
    }
  }

  const absCache = path.resolve(cacheDir);
  fs.mkdirSync(absCache, { recursive: true });
  const dest = assertPathInside(absCache, binaryName());
  refuseSymlinkPath(absCache, dest);
  if (binaryMatchesPin(dest, pin)) {
    return validateSpawnExecutable(dest, absCache);
  }

  const key = platformKey();
  if (!key || !pin.assets[key]) {
    throw new Error(
      `no pinned workerd asset for ${process.platform}-${process.arch}`,
    );
  }
  const asset = pin.assets[key]!;
  const url = validateFetchUrl(downloadUrl(pin, asset.artifact));
  console.error(`bookclerk-plugin: fetching ${url}`);
  // HTTPS (or loopback HTTP) URL validated by validateFetchUrl above.
  const res = await fetch(url);
  if (!res.ok) {
    throw new Error(`GET ${url} returned ${res.status}`);
  }
  const compressed = Buffer.from(await res.arrayBuffer());
  const got = createHash("sha256").update(compressed).digest("hex");
  if (got !== asset.sha256_hex) {
    throw new Error(
      `workerd download sha256 mismatch: got ${got}, expected ${asset.sha256_hex}`,
    );
  }

  const tmpName = `.${binaryName()}.tmp-${process.pid}-${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const tmp = assertPathInside(absCache, tmpName);
  let ownedTmp = false;
  try {
    const fd = fs.openSync(tmp, "wx");
    ownedTmp = true;
    try {
      const binary = gunzipSync(compressed);
      fs.writeFileSync(fd, binary);
      if (process.platform !== "win32") {
        fs.fchmodSync(fd, 0o755);
      }
    } finally {
      fs.closeSync(fd);
    }
    refuseSymlinkPath(absCache, dest);
    fs.renameSync(tmp, dest);
    ownedTmp = false;
  } catch (err) {
    if (ownedTmp) {
      try {
        fs.unlinkSync(tmp);
      } catch {
        /* ignore */
      }
    }
    throw err;
  }
  writeFileUnder(absCache, stampFileName(pin), `${pin.release_tag}\n`);
  console.error(
    `bookclerk-plugin: installed ${pin.release_tag} → ${dest}`,
  );
  return validateSpawnExecutable(dest, absCache);
}
