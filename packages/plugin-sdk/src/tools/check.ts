/**
 * Plugin tree checks and optional SDK embed sync for workerd guests.
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parse as parseToml } from "smol-toml";
import { validateLogo, validateManifest, type Manifest } from "./validate.js";

/**
 * Optional vendor filename for offline archives (host normally injects the package).
 */
export const EMBED_BOOKCLERK_PLUGIN_JS = "bookclerk_plugin.js";

/**
 * Resolves the path to the packaged workerd embed script.
 *
 * @returns Absolute path to `embed/bookclerk_plugin.js` inside this package.
 */
export function sdkEmbedSrc(): string {
  return path.resolve(
    path.dirname(fileURLToPath(import.meta.url)),
    "../../../embed/bookclerk_plugin.js",
  );
}

/**
 * Validates a plugin directory's `plugin.toml` and runtime assets.
 *
 * For workerd guests, also asserts the main module imports
 * `@bookclerk/plugin-sdk` / `BookclerkPlugin` rather than bare
 * `WorkerEntrypoint`.
 *
 * @param pluginDir - Plugin root containing `plugin.toml`.
 * @returns Human-readable success summary (`ok id=… kind=… runtime=…`).
 * @throws {Error} When the manifest or required assets are invalid / missing.
 *
 * @example
 * ```ts
 * console.log(checkPlugin("./my-plugin"));
 * // ok id=echo kind=source runtime=workerd
 * ```
 */
export function checkPlugin(pluginDir: string): string {
  const tomlPath = path.join(pluginDir, "plugin.toml");
  const text = fs.readFileSync(tomlPath, "utf8");
  const m = parseToml(text) as Manifest;
  validateManifest(m);
  if (m.logo != null) {
    const logo = validateLogo(String(m.logo));
    if (logo.kind === "embedded") {
      const logoPath = path.join(pluginDir, logo.value);
      if (!fs.existsSync(logoPath) || !fs.statSync(logoPath).isFile()) {
        throw new Error(`embedded logo missing: ${logoPath}`);
      }
    }
  }
  const runtime = m.runtime ?? "native";
  if (runtime === "workerd") {
    const modulesDir = path.join(pluginDir, m.workerd?.modules_dir ?? "modules");
    if (!fs.existsSync(modulesDir) || !fs.statSync(modulesDir).isDirectory()) {
      throw new Error(`workerd modules_dir missing: ${modulesDir}`);
    }
    const main = path.join(modulesDir, m.workerd!.main_module);
    if (!fs.existsSync(main)) {
      throw new Error(`workerd main_module missing: ${main}`);
    }
    const mainLower = m.workerd!.main_module.toLowerCase();
    if (mainLower.endsWith(".js") || mainLower.endsWith(".mjs")) {
      const src = fs.readFileSync(main, "utf8");
      const usesPackage =
        src.includes("@bookclerk/plugin-sdk") || src.includes("BookclerkPlugin");
      if (!usesPackage) {
        throw new Error(
          `${path.basename(main)}: import BookclerkPlugin from ` +
            `"@bookclerk/plugin-sdk/workerd" (or "@bookclerk/plugin-sdk")`,
        );
      }
      if (
        src.includes("WorkerEntrypoint") &&
        !src.includes("BookclerkPlugin") &&
        !src.includes("wasmBookclerkPlugin")
      ) {
        throw new Error(
          `${path.basename(main)}: subclass BookclerkPlugin from ` +
            `"@bookclerk/plugin-sdk/workerd", not bare WorkerEntrypoint`,
        );
      }
    }
  } else if (runtime === "native") {
    const cmd = m.command!;
    const resolved = path.isAbsolute(cmd) ? cmd : path.join(pluginDir, cmd);
    if (
      !fs.existsSync(resolved) &&
      fs.existsSync(path.join(pluginDir, ".require-binary"))
    ) {
      throw new Error(`native command not found: ${resolved}`);
    }
  }
  return `ok id=${m.id} kind=${m.kind} runtime=${runtime}`;
}

/**
 * Optionally vendors the workerd embed under the plugin modules tree.
 *
 * Prefer `import { BookclerkPlugin } from "@bookclerk/plugin-sdk/workerd"` —
 * `bookclerk-workerd` injects that module at runtime. This helper remains for
 * offline / air-gapped archives.
 *
 * @param pluginDir - Workerd plugin root containing `plugin.toml`.
 * @returns Human-readable sync summary including the destination path.
 * @throws {Error} When the runtime is not workerd or the embed source is missing.
 *
 * @example
 * ```ts
 * console.log(syncEmbed("./my-workerd-plugin"));
 * ```
 */
export function syncEmbed(pluginDir: string): string {
  const tomlPath = path.join(pluginDir, "plugin.toml");
  const m = parseToml(fs.readFileSync(tomlPath, "utf8")) as Manifest;
  validateManifest(m);
  if ((m.runtime ?? "native") !== "workerd") {
    throw new Error('sync-embed requires runtime = "workerd"');
  }
  const main = m.workerd!.main_module.toLowerCase();
  if (!main.endsWith(".js") && !main.endsWith(".mjs")) {
    throw new Error(
      `sync-embed (TypeScript SDK): main_module must be .js/.mjs (got ${m.workerd!.main_module})`,
    );
  }
  const modulesDir = path.join(pluginDir, m.workerd?.modules_dir ?? "modules");
  const destDir = path.join(modulesDir, "@bookclerk", "plugin-sdk");
  fs.mkdirSync(destDir, { recursive: true });
  const dest = path.join(destDir, "workerd.js");
  const src = sdkEmbedSrc();
  if (!fs.existsSync(src)) {
    throw new Error(`SDK embed missing: ${src}`);
  }
  fs.copyFileSync(src, dest);
  return `synced ${dest} (optional vendor; prefer package import + bookclerk-workerd inject)`;
}
