#!/usr/bin/env node
/**
 * Experimental Node SEA pack sketch.
 * See https://nodejs.org/api/single-executable-applications.html
 *
 * Official Node SEA CI coverage today: Windows, Linux (glibc, not Alpine),
 * macOS arm64. macOS x64 and musl/Alpine are unsupported gaps — do not claim
 * parity with the Rust Echo guest.
 */
import { execFileSync } from "node:child_process";
import { copyFileSync, chmodSync, existsSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import process from "node:process";

const outDir = join("dist", "sea");
mkdirSync(outDir, { recursive: true });

const isWin = process.platform === "win32";
const exeName = isWin ? "bookclerk-plugin-echo-ts.exe" : "bookclerk-plugin-echo-ts";
const exePath = join(outDir, exeName);

copyFileSync(process.execPath, exePath);
if (!isWin) chmodSync(exePath, 0o755);

execFileSync(process.execPath, ["--experimental-sea-config", "sea-config.json"], {
  stdio: "inherit",
});

if (!existsSync("sea-prep.blob")) {
  console.error("sea-prep.blob missing — Node SEA prep failed");
  process.exit(1);
}

if (process.platform === "darwin") {
  execFileSync("codesign", ["--remove-signature", exePath], { stdio: "inherit" });
}

// Inject the SEA blob (postject). Publishers must install postject and follow
// the current Node docs — flags change across Node majors.
console.error(
  "Experimental: run postject per Node SEA docs to inject sea-prep.blob into",
  exePath,
);
console.error("Then pack plugin.toml + binary as {crate}-{version}-{bookclerk_target}.{ext}");
