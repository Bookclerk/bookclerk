/**
 * Packages a plugin directory into a distributable `.tar.gz` archive.
 */

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { parse as parseToml } from "smol-toml";
import { validateLogo, validateManifest, type Manifest } from "./validate.js";

function hostTarget(): string {
  const plat = process.platform;
  const arch = process.arch;
  if (plat === "linux" && arch === "x64") return "linux-x64-gnu";
  if (plat === "linux" && arch === "arm64") return "linux-arm64";
  if (plat === "darwin" && arch === "arm64") return "macos-arm64";
  if (plat === "darwin" && arch === "x64") return "macos-x64";
  if (plat === "win32" && arch === "x64") return "windows-x64";
  return `${plat}-${arch}`;
}

function copyRecursive(src: string, dst: string): void {
  fs.mkdirSync(dst, { recursive: true });
  for (const ent of fs.readdirSync(src, { withFileTypes: true })) {
    const from = path.join(src, ent.name);
    const to = path.join(dst, ent.name);
    if (ent.isDirectory()) copyRecursive(from, to);
    else fs.copyFileSync(from, to);
  }
}

/**
 * Packages `pluginDir` into `outDir` and returns the archive path.
 *
 * Native archives include the command binary and are tagged with the host
 * target triple. Workerd archives include the modules tree (the SDK is injected
 * by `bookclerk-workerd` at serve time). Updates `SHA256SUMS` beside the
 * archive.
 *
 * @param pluginDir - Plugin root containing `plugin.toml`.
 * @param outDir - Destination directory for the archive and checksums.
 * @returns Absolute path to the created `.tar.gz`.
 * @throws {Error} When validation fails, assets are missing, or `tar` fails.
 *
 * @example
 * ```ts
 * const archive = packagePlugin("./my-plugin", "./dist");
 * console.log(`packed ${archive}`);
 * ```
 */
export function packagePlugin(pluginDir: string, outDir: string): string {
  const tomlPath = path.join(pluginDir, "plugin.toml");
  const m = parseToml(fs.readFileSync(tomlPath, "utf8")) as Manifest;
  validateManifest(m);
  const version = m.version ?? "0.0.0";
  const id = m.id;
  fs.mkdirSync(outDir, { recursive: true });
  const staging = path.join(outDir, `.staging-${id}`);
  fs.rmSync(staging, { recursive: true, force: true });
  fs.mkdirSync(staging, { recursive: true });
  fs.copyFileSync(tomlPath, path.join(staging, "plugin.toml"));

  if (m.logo != null) {
    const logo = validateLogo(String(m.logo));
    if (logo.kind === "embedded") {
      const src = path.join(pluginDir, logo.value);
      if (!fs.existsSync(src) || !fs.statSync(src).isFile()) {
        throw new Error(`embedded logo missing for package: ${src}`);
      }
      const dest = path.join(staging, logo.value);
      fs.mkdirSync(path.dirname(dest), { recursive: true });
      fs.copyFileSync(src, dest);
    }
  }

  const runtime = m.runtime ?? "native";
  let archiveStem: string;
  if (runtime === "native") {
    const cmd = m.command!;
    const src = path.isAbsolute(cmd) ? cmd : path.join(pluginDir, cmd);
    if (!fs.existsSync(src)) {
      throw new Error(`native binary not found for package: ${src}`);
    }
    const binName = path.basename(src);
    fs.copyFileSync(src, path.join(staging, binName));
    try {
      fs.chmodSync(path.join(staging, binName), 0o755);
    } catch {
      /* windows */
    }
    archiveStem = `bookclerk-plugin-${id}-${version}-${hostTarget()}`;
  } else {
    const modulesDir = m.workerd?.modules_dir ?? "modules";
    copyRecursive(
      path.join(pluginDir, modulesDir),
      path.join(staging, modulesDir),
    );
    // Authors import `@bookclerk/plugin-sdk/workerd`; bookclerk-workerd injects it.
    archiveStem = `bookclerk-plugin-${id}-${version}-workerd`;
  }

  const archiveName = `${archiveStem}.tar.gz`;
  const archivePath = path.join(outDir, archiveName);
  const tar = spawnSync(
    "tar",
    ["-C", staging, "-czf", archivePath, "."],
    { encoding: "utf8" },
  );
  if (tar.status !== 0) {
    throw new Error(`tar failed: ${tar.stderr || tar.stdout}`);
  }
  fs.rmSync(staging, { recursive: true, force: true });

  const digest = crypto
    .createHash("sha256")
    .update(fs.readFileSync(archivePath))
    .digest("hex");
  const sumsPath = path.join(outDir, "SHA256SUMS");
  let body = "";
  if (fs.existsSync(sumsPath)) {
    body = fs
      .readFileSync(sumsPath, "utf8")
      .split("\n")
      .filter((l) => l && !l.endsWith(archiveName))
      .join("\n");
    if (body && !body.endsWith("\n")) body += "\n";
  }
  body += `${digest}  ${archiveName}\n`;
  fs.writeFileSync(sumsPath, body);
  return archivePath;
}
