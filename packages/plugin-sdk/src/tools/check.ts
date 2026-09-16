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
 * Exported class name the launcher binds for each `entrypoints` wire name.
 *
 * Mirrors `ENTRYPOINT_CLASSES` in `plugin.ts` (kept local so the tools bundle
 * does not import the Workers runtime module).
 */
export const ENTRYPOINT_EXPORT_CLASSES: Readonly<Record<string, string>> = Object.freeze({
  storefront: "Storefront",
  storage: "Storage",
  databaseAdapter: "DatabaseAdapter",
  remoteLibrary: "RemoteLibrary",
  cli: "Cli",
  oidc: "Oidc",
});

/**
 * Checks a workerd main module against the v3 author model.
 *
 * Requires the `@bookclerk/plugin-sdk` import and a `BookclerkEntrypoint`
 * default export; rejects the removed `BookclerkPlugin` base; and requires an
 * exported class per manifest entrypoint (`export class Storage …`). Python
 * mains apply the same rules with `class Storage(` syntax.
 *
 * @param mainName - Main module filename (for messages).
 * @param src - Main module source text.
 * @param entrypoints - Manifest `entrypoints` wire names.
 * @param language - `js` or `python`.
 * @throws {Error} When the module does not follow the author model.
 */
export function checkMainModuleSource(
  mainName: string,
  src: string,
  entrypoints: readonly string[],
  language: "js" | "python",
): void {
  const base = "BookclerkEntrypoint";
  if (src.includes("BookclerkPlugin")) {
    throw new Error(
      `${mainName}: \`BookclerkPlugin\` was removed in api_version 3; extend ` +
        `\`${base}\` (default export with event()/job() triggers) and export ` +
        `named entrypoint classes (Storefront, Storage, RemoteLibrary, DatabaseAdapter, Cli, Oidc)`,
    );
  }
  if (language === "js") {
    const usesPackage = src.includes("@bookclerk/plugin-sdk") || src.includes(base);
    if (!usesPackage) {
      throw new Error(
        `${mainName}: import ${base} from "@bookclerk/plugin-sdk/workerd" (or "@bookclerk/plugin-sdk")`,
      );
    }
    if (src.includes("WorkerEntrypoint") && !src.includes(base) && !src.includes("Entrypoint")) {
      throw new Error(
        `${mainName}: subclass ${base} from "@bookclerk/plugin-sdk/workerd", not bare WorkerEntrypoint`,
      );
    }
  } else {
    const usesPackage = src.includes("bookclerk_plugin_sdk") || src.includes(base);
    if (!usesPackage) {
      throw new Error(`${mainName}: import ${base} from bookclerk_plugin_sdk.workerd`);
    }
  }
  for (const wire of entrypoints) {
    const cls = ENTRYPOINT_EXPORT_CLASSES[wire];
    if (!cls) continue;
    const exported =
      language === "js"
        ? new RegExp(`export\\s+class\\s+${cls}\\b`).test(src) ||
          new RegExp(`export\\s*\\{[^}]*\\b${cls}\\b[^}]*\\}`).test(src)
        : new RegExp(`^class\\s+${cls}\\s*\\(`, "m").test(src);
    if (!exported) {
      const hint =
        language === "js"
          ? `export class ${cls} extends ${cls}Entrypoint`
          : `class ${cls}(${cls}Entrypoint)`;
      throw new Error(
        `${mainName}: entrypoint \`${wire}\` declared in plugin.toml but the main module does not export \`${cls}\` (${hint})`,
      );
    }
  }
}

/**
 * Validates a plugin directory's `plugin.toml` and runtime assets.
 *
 * For workerd guests, also asserts the main module follows the v3 author
 * model (see {@link checkMainModuleSource}).
 *
 * @param pluginDir - Plugin root containing `plugin.toml`.
 * @returns Human-readable success summary (`ok id=… entrypoints=… runtime=…`).
 * @throws {Error} When the manifest or required assets are invalid / missing.
 *
 * @example
 * ```ts
 * console.log(checkPlugin("./my-plugin"));
 * // ok id=echo entrypoints=storefront runtime=workerd
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
    const entrypoints = m.entrypoints ?? [];
    if (mainLower.endsWith(".js") || mainLower.endsWith(".mjs")) {
      const src = fs.readFileSync(main, "utf8");
      checkMainModuleSource(path.basename(main), src, entrypoints, "js");
    } else if (mainLower.endsWith(".py")) {
      const src = fs.readFileSync(main, "utf8");
      checkMainModuleSource(path.basename(main), src, entrypoints, "python");
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
  return `ok id=${m.id} entrypoints=${(m.entrypoints ?? []).join(",")} runtime=${runtime}`;
}

/**
 * Optionally vendors the workerd embed under the plugin modules tree.
 *
 * Prefer `import { BookclerkEntrypoint } from "@bookclerk/plugin-sdk/workerd"` —
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
