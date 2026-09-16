#!/usr/bin/env node
/**
 * Bundle `dist/workerd.js` into the single-file isolate embed
 * `embed/bookclerk_plugin.js`.
 *
 * `bookclerk-workerd` injects the embed into every plugin isolate under the
 * `@bookclerk/plugin-sdk/workerd` and `@bookclerk/plugin-sdk` module names,
 * so it must be one self-contained ES module: the author runtime
 * (`plugin.ts`), the generated Cap'n Proto codecs (`generated-wire.ts`) the
 * adapter decodes `/invoke` bodies with, and the typed SQL helpers. Only
 * `cloudflare:workers` stays external.
 *
 * The output is committed; CI rebuilds it and fails on drift. The Rust SDK
 * crate and the Python SDK ship byte-identical mirrors kept in step by
 * `scripts/gen-plugin-abi.py` / `scripts/sync-workerd-pin.py`.
 */
import { build } from "esbuild";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const pkg = JSON.parse(readFileSync(join(root, "../package.json"), "utf8"));

const banner = `/**
 * GENERATED FILE - do not edit. Bundled from \`src/workerd.ts\` by
 * \`scripts/build-embed.mjs\` (\`npm run build\` in packages/plugin-sdk).
 *
 * Workerd runtime for \`@bookclerk/plugin-sdk\` / \`@bookclerk/plugin-sdk/workerd\`
 * (${pkg.name}@${pkg.version}). \`bookclerk-workerd\` injects this module into every
 * plugin isolate under those names; authors import the package, never a
 * relative embed path. Native guests use Rust \`serve\` / \`PluginWorker\` instead.
 */`;

await build({
  entryPoints: [join(root, "../dist/workerd.js")],
  outfile: join(root, "../embed/bookclerk_plugin.js"),
  bundle: true,
  format: "esm",
  platform: "neutral",
  target: "es2022",
  // `node:*` only appears behind runtime feature checks that never fire in
  // workerd (`crypto.subtle` is always present there).
  external: ["cloudflare:workers", "node:*"],
  legalComments: "none",
  // Keep the bundle readable and byte-stable across machines.
  minify: false,
  sourcemap: false,
  charset: "utf8",
  logLevel: "warning",
  banner: { js: banner },
});

console.log("bundled embed/bookclerk_plugin.js");
