/**
 * Download / refresh the pinned Cloudflare `workerd` binary (mirrors ensure.rs).
 */

import { createHash } from "node:crypto";
import fs from "node:fs";
import { createGunzip } from "node:zlib";
import { pipeline } from "node:stream/promises";
import { Readable } from "node:stream";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import os from "node:os";

export type WorkerdAsset = {
  artifact: string;
  sha256_hex: string;
};

export type WorkerdPin = {
  release_tag: string;
  bundled_compat_date: string;
  version_stamp: string;
  assets: Record<string, WorkerdAsset>;
};

/** Package root (`packages/plugin-sdk`) whether running from `src/` or `dist/`. */
export function packageRoot(): string {
  const here = path.dirname(fileURLToPath(import.meta.url));
  // dist/sparse-workerd or src/sparse-workerd → package root
  return path.resolve(here, "../..");
}

export function loadPin(root = packageRoot()): WorkerdPin {
  const pinPath = path.join(root, "workerd-pin.json");
  return JSON.parse(fs.readFileSync(pinPath, "utf8")) as WorkerdPin;
}

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

export function binaryName(): string {
  return process.platform === "win32" ? "workerd.exe" : "workerd";
}

export function downloadUrl(pin: WorkerdPin, artifact: string): string {
  return `https://github.com/cloudflare/workerd/releases/download/${pin.release_tag}/${artifact}`;
}

/** Preferred cache: `BOOKCLERK_WORKERD_CACHE` or `~/.cache/bookclerk/workerd`. */
export function defaultCacheDir(): string {
  if (process.env.BOOKCLERK_WORKERD_CACHE) {
    return process.env.BOOKCLERK_WORKERD_CACHE;
  }
  const home = os.homedir();
  return path.join(home, ".cache", "bookclerk", "workerd");
}

function isCurrent(bin: string, pin: WorkerdPin): boolean {
  const dir = path.dirname(bin);
  const stamp = path.join(dir, pin.version_stamp);
  if (fs.existsSync(stamp)) {
    const text = fs.readFileSync(stamp, "utf8").trim();
    if (text === pin.release_tag) return true;
  }
  const out = spawnSync(bin, ["--version"], { encoding: "utf8" });
  if (out.status !== 0) return false;
  const combined = `${out.stdout ?? ""}${out.stderr ?? ""}`;
  const pinBare = pin.release_tag.replace(/^v/, "");
  return combined.includes(pin.release_tag) || combined.includes(pinBare);
}

/**
 * Ensure `cacheDir/workerd` matches the pin, downloading if needed.
 * Honors `BOOKCLERK_WORKERD_BIN` when set and current.
 */
export async function ensureWorkerd(
  cacheDir = defaultCacheDir(),
  root = packageRoot(),
): Promise<string> {
  const pin = loadPin(root);
  const override = process.env.BOOKCLERK_WORKERD_BIN;
  if (override && fs.existsSync(override) && isCurrent(override, pin)) {
    return override;
  }

  fs.mkdirSync(cacheDir, { recursive: true });
  const dest = path.join(cacheDir, binaryName());
  if (fs.existsSync(dest) && isCurrent(dest, pin)) {
    return dest;
  }

  const key = platformKey();
  if (!key || !pin.assets[key]) {
    throw new Error(
      `no pinned workerd asset for ${process.platform}-${process.arch}`,
    );
  }
  const asset = pin.assets[key]!;
  const url = downloadUrl(pin, asset.artifact);
  console.error(`bookclerk-plugin: fetching ${url}`);
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

  const tmp = path.join(cacheDir, `${binaryName()}.tmp`);
  await pipeline(
    Readable.from(compressed),
    createGunzip(),
    fs.createWriteStream(tmp),
  );
  if (process.platform !== "win32") {
    fs.chmodSync(tmp, 0o755);
  }
  fs.renameSync(tmp, dest);
  fs.writeFileSync(path.join(cacheDir, pin.version_stamp), `${pin.release_tag}\n`);
  console.error(
    `bookclerk-plugin: installed ${pin.release_tag} → ${dest}`,
  );
  return dest;
}
