#!/usr/bin/env node
/** Symlink containment regressions for package + sparse-workerd helpers. */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

function mustThrow(label, fn, pattern = /symlink|\.\.|unsafe|escape|must not contain/i) {
  let threw = false;
  try {
    fn();
  } catch (e) {
    threw = pattern.test(String(e));
    if (!threw) console.error(`unexpected ${label} error:`, e);
  }
  if (!threw) {
    console.error(`FAIL: ${label}`);
    process.exit(1);
  }
  console.log(`ok ${label}`);
}

async function main() {
  let assertPathInside, packagePlugin, materializeConfig, syncEmbed;
  try {
    ({ assertPathInside } = await import("../dist/sparse-workerd/ensure.js"));
    ({ packagePlugin } = await import("../dist/tools/package.js"));
    ({ materializeConfig } = await import("../dist/sparse-workerd/config.js"));
    ({ syncEmbed } = await import("../dist/tools/check.js"));
  } catch {
    console.error("dist/ missing — run npm run build first");
    process.exit(1);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "bc-path-"));
  const plugin = path.join(tmp, "plugin");
  const modules = path.join(plugin, "modules");
  fs.mkdirSync(modules, { recursive: true });
  const manifest = {
    api_version: 3,
    id: "sym_test",
    runtime: "workerd",
    entrypoints: ["cli"],
    workerd: {
      compatibility_date: "2026-08-01",
      main_module: "main.js",
      modules_dir: "modules",
      entrypoint: "default",
    },
    capabilities: { network: { mode: "deny" } },
  };
  fs.writeFileSync(
    path.join(plugin, "plugin.toml"),
    `api_version = 3
id = "sym_test"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "main.js"
modules_dir = "modules"
entrypoint = "default"
[capabilities.network]
mode = "deny"
`,
  );
  fs.writeFileSync(path.join(modules, "main.js"), "export default class X {}");
  const outside = path.join(tmp, "outside.txt");
  fs.writeFileSync(outside, "SECRET_OUTSIDE");
  fs.symlinkSync(outside, path.join(modules, "leak.txt"));

  mustThrow("packagePlugin refuses module symlink", () =>
    packagePlugin(plugin, path.join(tmp, "dist")),
  );

  // Outside TOML via leaf symlink must be refused before archive publish.
  const tomlOutside = path.join(tmp, "evil-plugin.toml");
  fs.copyFileSync(path.join(plugin, "plugin.toml"), tomlOutside);
  fs.rmSync(path.join(plugin, "plugin.toml"));
  fs.symlinkSync(tomlOutside, path.join(plugin, "plugin.toml"));
  mustThrow("packagePlugin refuses plugin.toml leaf symlink", () =>
    packagePlugin(plugin, path.join(tmp, "dist-toml")),
  );
  fs.rmSync(path.join(plugin, "plugin.toml"));
  fs.copyFileSync(tomlOutside, path.join(plugin, "plugin.toml"));

  // syncEmbed: modules dir / leaf embed symlinks refused; nested OK; symlinked root OK.
  mustThrow("syncEmbed refuses modules dir symlink", () => {
    const p = path.join(tmp, "sync-mod");
    fs.mkdirSync(path.join(p, "modules"), { recursive: true });
    fs.copyFileSync(path.join(plugin, "plugin.toml"), path.join(p, "plugin.toml"));
    fs.writeFileSync(path.join(p, "modules", "main.js"), "export default class X {}");
    fs.rmSync(path.join(p, "modules"), { recursive: true, force: true });
    fs.symlinkSync(path.join(tmp, "outside-mod"), path.join(p, "modules"));
    fs.mkdirSync(path.join(tmp, "outside-mod"), { recursive: true });
    syncEmbed(p);
  });

  // modules/@bookclerk -> outside with missing plugin-sdk must not mkdir outside.
  const syncEscape = path.join(tmp, "sync-escape");
  const outsideBookclerk = path.join(tmp, "outside-bookclerk");
  fs.mkdirSync(path.join(syncEscape, "modules"), { recursive: true });
  fs.mkdirSync(outsideBookclerk, { recursive: true });
  fs.copyFileSync(path.join(plugin, "plugin.toml"), path.join(syncEscape, "plugin.toml"));
  fs.writeFileSync(path.join(syncEscape, "modules", "main.js"), "export default class X {}");
  fs.symlinkSync(outsideBookclerk, path.join(syncEscape, "modules", "@bookclerk"));
  const outsideBefore = fs.readdirSync(outsideBookclerk);
  mustThrow("syncEmbed refuses @bookclerk symlink before mkdir", () =>
    syncEmbed(syncEscape),
  );
  const outsideAfter = fs.readdirSync(outsideBookclerk);
  if (JSON.stringify(outsideBefore) !== JSON.stringify(outsideAfter)) {
    console.error("FAIL: outside-bookclerk was mutated by syncEmbed", outsideAfter);
    process.exit(1);
  }
  if (fs.existsSync(path.join(outsideBookclerk, "plugin-sdk"))) {
    console.error("FAIL: outside/plugin-sdk was created through @bookclerk symlink");
    process.exit(1);
  }
  console.log("ok syncEmbed refuses @bookclerk symlink before mkdir");

  // Absent default modules/ must be created (ordinary first-time sync-embed).
  const syncAbsent = path.join(tmp, "sync-absent");
  fs.mkdirSync(syncAbsent, { recursive: true });
  fs.copyFileSync(path.join(plugin, "plugin.toml"), path.join(syncAbsent, "plugin.toml"));
  if (fs.existsSync(path.join(syncAbsent, "modules"))) {
    console.error("FAIL: sync-absent fixture must start without modules/");
    process.exit(1);
  }
  const syncedAbsent = syncEmbed(syncAbsent);
  if (!syncedAbsent.includes("synced")) {
    console.error("FAIL: syncEmbed absent modules", syncedAbsent);
    process.exit(1);
  }
  if (
    !fs.existsSync(
      path.join(syncAbsent, "modules", "@bookclerk", "plugin-sdk", "workerd.js"),
    )
  ) {
    console.error("FAIL: syncEmbed did not create modules/@bookclerk/plugin-sdk/workerd.js");
    process.exit(1);
  }
  console.log("ok syncEmbed creates absent default modules");

  // Nested missing modules_dir path must also be created under the plugin root.
  const syncNested = path.join(tmp, "sync-nested");
  fs.mkdirSync(syncNested, { recursive: true });
  fs.writeFileSync(
    path.join(syncNested, "plugin.toml"),
    `api_version = 3
id = "sym_nested"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "main.js"
modules_dir = "mods/nested"
entrypoint = "default"
[capabilities.network]
mode = "deny"
`,
  );
  const syncedNested = syncEmbed(syncNested);
  if (!syncedNested.includes("synced")) {
    console.error("FAIL: syncEmbed nested modules path", syncedNested);
    process.exit(1);
  }
  if (
    !fs.existsSync(
      path.join(syncNested, "mods", "nested", "@bookclerk", "plugin-sdk", "workerd.js"),
    )
  ) {
    console.error("FAIL: syncEmbed did not create mods/nested/@bookclerk/plugin-sdk/workerd.js");
    process.exit(1);
  }
  console.log("ok syncEmbed creates nested missing modules path");

  const syncPlugin = path.join(tmp, "sync-plugin");
  fs.mkdirSync(path.join(syncPlugin, "modules"), { recursive: true });
  fs.copyFileSync(path.join(plugin, "plugin.toml"), path.join(syncPlugin, "plugin.toml"));
  fs.writeFileSync(path.join(syncPlugin, "modules", "main.js"), "export default class X {}");
  const embedVictim = path.join(tmp, "embed-victim.js");
  fs.writeFileSync(embedVictim, "SECRET_EMBED");
  fs.mkdirSync(path.join(syncPlugin, "modules", "@bookclerk", "plugin-sdk"), {
    recursive: true,
  });
  fs.symlinkSync(
    embedVictim,
    path.join(syncPlugin, "modules", "@bookclerk", "plugin-sdk", "workerd.js"),
  );
  mustThrow("syncEmbed refuses workerd leaf symlink", () => syncEmbed(syncPlugin));
  if (fs.readFileSync(embedVictim, "utf8") !== "SECRET_EMBED") {
    console.error("FAIL: embed victim was modified");
    process.exit(1);
  }
  fs.rmSync(path.join(syncPlugin, "modules", "@bookclerk"), { recursive: true, force: true });
  const synced = syncEmbed(syncPlugin);
  if (!synced.includes("synced")) {
    console.error("FAIL: syncEmbed nested modules", synced);
    process.exit(1);
  }
  console.log("ok syncEmbed writes nested modules");

  const syncLink = path.join(tmp, "sync-link");
  fs.symlinkSync(syncPlugin, syncLink);
  const syncedLink = syncEmbed(syncLink);
  if (!syncedLink.includes("synced")) {
    console.error("FAIL: syncEmbed symlinked root", syncedLink);
    process.exit(1);
  }
  console.log("ok syncEmbed allows symlinked plugin root");

  const draftRoot = path.join(tmp, "draftroot");
  fs.mkdirSync(draftRoot);
  const draft = assertPathInside(draftRoot, "..draft");
  if (!draft.endsWith(`${path.sep}..draft`)) {
    console.error("FAIL: ..draft rejected", draft);
    process.exit(1);
  }
  console.log("ok assertPathInside allows ..draft name");

  const outsideDir = path.join(tmp, "out_dir");
  fs.mkdirSync(outsideDir);
  fs.symlinkSync(outsideDir, path.join(plugin, ".bookclerk"));
  fs.rmSync(path.join(modules, "leak.txt"));
  // Generated embeds go to a host session dir — a `.bookclerk` symlink under the
  // plugin root must not receive bridge assets (and must not block materialize).
  const generated = materializeConfig(plugin, manifest, {
    listenPort: 0,
    bridgeToken: "tok",
  });
  if (fs.existsSync(path.join(outsideDir, "bridge.js"))) {
    console.error("FAIL: outside bridge.js was written");
    process.exit(1);
  }
  if (!generated.stateDir || generated.stateDir === plugin) {
    console.error("FAIL: expected session stateDir distinct from plugin root");
    process.exit(1);
  }
  if (!fs.existsSync(path.join(generated.stateDir, ".bookclerk", "bridge.js"))) {
    console.error("FAIL: bridge.js missing under session stateDir");
    process.exit(1);
  }
  console.log("ok materializeConfig writes under session dir (ignores plugin .bookclerk symlink)");
  fs.rmSync(generated.stateDir, { recursive: true, force: true });

  fs.rmSync(path.join(plugin, ".bookclerk"), { force: true });
  fs.rmSync(path.join(modules, "main.js"));
  fs.symlinkSync(outside, path.join(modules, "main.js"));
  mustThrow("materializeConfig refuses main module symlink", () =>
    materializeConfig(plugin, manifest, { listenPort: 0, bridgeToken: "tok" }),
  );

  // Intermediate directory symlink under modules/.
  fs.rmSync(path.join(modules, "main.js"), { force: true });
  fs.writeFileSync(path.join(modules, "main.js"), "export default class X {}");
  const nestedOutside = path.join(tmp, "nested_out");
  fs.mkdirSync(nestedOutside);
  fs.writeFileSync(path.join(nestedOutside, "x.js"), "export default 1");
  fs.symlinkSync(nestedOutside, path.join(modules, "vendor"));
  mustThrow("packagePlugin refuses intermediate modules dir symlink", () =>
    packagePlugin(plugin, path.join(tmp, "dist-mid")),
  );

  // Manifest `..` path via assertPathInside (same contract package uses).
  mustThrow(
    "assertPathInside rejects .. manifest path",
    () => assertPathInside(plugin, "../outside.txt"),
    /\.\.|escape|must not contain/i,
  );

  // Symlinked plugin root must still package a valid tree.
  fs.rmSync(path.join(modules, "vendor"), { force: true });
  const linkPlugin = path.join(tmp, "link_plugin");
  fs.symlinkSync(plugin, linkPlugin);
  const archive = packagePlugin(linkPlugin, path.join(tmp, "dist-link"));
  if (!fs.existsSync(archive)) {
    console.error("FAIL: symlinked plugin root did not package");
    process.exit(1);
  }
  console.log("ok packagePlugin allows symlinked plugin root");

  // Manifest version with `..` must not escape outDir or delete outside files.
  const travPlugin = path.join(tmp, "trav_plugin");
  fs.mkdirSync(path.join(travPlugin, "modules"), { recursive: true });
  fs.writeFileSync(
    path.join(travPlugin, "plugin.toml"),
    `api_version = 3
id = "probe"
version = "../../../victim"
runtime = "workerd"
entrypoints = ["cli"]
[workerd]
compatibility_date = "2026-08-01"
main_module = "main.js"
modules_dir = "modules"
entrypoint = "default"
[capabilities.network]
mode = "deny"
`,
  );
  fs.writeFileSync(
    path.join(travPlugin, "modules", "main.js"),
    "export default class X {}",
  );
  const travOut = path.join(tmp, "trav_out");
  fs.mkdirSync(travOut);
  const victim = path.join(tmp, "victim-workerd.tar.gz");
  fs.writeFileSync(victim, "PREEXISTING");
  mustThrow(
    "packagePlugin refuses version path traversal",
    () => packagePlugin(travPlugin, travOut),
    /\.\.|escape|must not contain/i,
  );
  if (fs.readFileSync(victim, "utf8") !== "PREEXISTING") {
    console.error("FAIL: outside victim archive was modified");
    process.exit(1);
  }

  // Tar failure must not delete a pre-existing final archive under outDir.
  const failOut = path.join(tmp, "fail_out");
  fs.mkdirSync(failOut);
  const good = packagePlugin(plugin, failOut);
  fs.writeFileSync(good, "KEEP_FINAL");
  const prevPath = process.env.PATH;
  process.env.PATH = "";
  mustThrow(
    "packagePlugin tar failure with empty PATH",
    () => packagePlugin(plugin, failOut),
    /tar failed|spawn|ENOENT|status/i,
  );
  process.env.PATH = prevPath;
  if (fs.readFileSync(good, "utf8") !== "KEEP_FINAL") {
    console.error("FAIL: pre-existing final archive was deleted on tar failure");
    process.exit(1);
  }
  console.log("ok packagePlugin preserves final archive when tar fails");

  const state = fs.mkdtempSync(path.join(tmp, "supplied-state-"));
  const bookclerk = path.join(state, ".bookclerk");
  fs.mkdirSync(bookclerk);
  const adapterVictim = path.join(tmp, "victim-adapter.js");
  fs.writeFileSync(adapterVictim, "VICTIM");
  fs.symlinkSync(adapterVictim, path.join(bookclerk, "adapter.js"));
  const leafPlugin = path.join(tmp, "leaf_plugin");
  const leafModules = path.join(leafPlugin, "modules");
  fs.mkdirSync(leafModules, { recursive: true });
  fs.writeFileSync(path.join(leafModules, "main.js"), "export default class X {}");
  mustThrow("materializeConfig refuses leaf symlink in supplied stateDir", () =>
    materializeConfig(leafPlugin, manifest, {
      listenPort: 0,
      bridgeToken: "tok",
      stateDir: state,
    }),
  );
  if (fs.readFileSync(adapterVictim, "utf8") !== "VICTIM") {
    console.error("FAIL: supplied stateDir leaf symlink overwrote victim");
    process.exit(1);
  }
  console.log("ok materializeConfig refuses leaf symlink in supplied stateDir");

  const nested = path.join(tmp, "nested_plugin");
  const nestedFile = path.join(nested, "dist", "modules", "nested", "edition..2.js");
  fs.mkdirSync(path.dirname(nestedFile), { recursive: true });
  fs.writeFileSync(nestedFile, "export default class Nested {}");
  const nestedManifest = {
    ...manifest,
    id: "nested",
    workerd: {
      ...manifest.workerd,
      modules_dir: "dist/modules",
      main_module: "nested/edition..2.js",
    },
  };
  const nestedGenerated = materializeConfig(nested, nestedManifest, {
    listenPort: 0,
    bridgeToken: "tok",
  });
  const nestedConfig = fs.readFileSync(nestedGenerated.configPath, "utf8");
  if (!nestedConfig.includes("/dist/modules/nested/edition..2.js")) {
    console.error("FAIL: nested module embed missing", nestedConfig);
    process.exit(1);
  }
  console.log("ok materializeConfig allows nested modules and edition..2.js");
  fs.rmSync(nestedGenerated.stateDir, { recursive: true, force: true });

  const { writeFileUnder, defaultCacheDir, binaryMatchesPin, loadPin } = await import(
    "../dist/sparse-workerd/ensure.js"
  );
  const writeRoot = path.join(tmp, "write-root");
  fs.mkdirSync(writeRoot);
  const written = writeFileUnder(writeRoot, "edition..2.js", "ok");
  if (fs.readFileSync(written, "utf8") !== "ok") {
    console.error("FAIL: edition..2.js was not written");
    process.exit(1);
  }
  const linkVictim = path.join(tmp, "link-victim");
  fs.writeFileSync(linkVictim, "KEEP");
  fs.symlinkSync(linkVictim, path.join(writeRoot, "adapter.js"));
  mustThrow("writeFileUnder refuses leaf symlink", () =>
    writeFileUnder(writeRoot, "adapter.js", "NEW"),
  );
  if (fs.readFileSync(linkVictim, "utf8") !== "KEEP") {
    console.error("FAIL: writeFileUnder followed leaf symlink");
    process.exit(1);
  }
  console.log("ok writeFileUnder refuses leaf symlink and allows edition..2.js");

  const prevCache = process.env.BOOKCLERK_WORKERD_CACHE;
  const cacheOutside = path.join(tmp, "wd-cache");
  process.env.BOOKCLERK_WORKERD_CACHE = cacheOutside;
  if (path.resolve(defaultCacheDir()) !== path.resolve(cacheOutside)) {
    console.error("FAIL: BOOKCLERK_WORKERD_CACHE was not honored", defaultCacheDir());
    process.exit(1);
  }
  if (prevCache === undefined) delete process.env.BOOKCLERK_WORKERD_CACHE;
  else process.env.BOOKCLERK_WORKERD_CACHE = prevCache;
  console.log("ok defaultCacheDir honors cache outside home");

  const pin = loadPin();
  const stampDir = path.join(tmp, "stamp-only");
  fs.mkdirSync(stampDir);
  fs.writeFileSync(path.join(stampDir, pin.version_stamp), `${pin.release_tag}\n`);
  const missingBin = path.join(stampDir, "workerd");
  if (binaryMatchesPin(missingBin, pin)) {
    console.error("FAIL: stamp without binary was treated as current");
    process.exit(1);
  }
  const stampedBin = path.join(stampDir, "present-workerd");
  fs.writeFileSync(stampedBin, "not-executable-needed");
  if (!binaryMatchesPin(stampedBin, pin)) {
    console.error("FAIL: stamped regular file was not current");
    process.exit(1);
  }
  const stampedLink = path.join(stampDir, "link-workerd");
  fs.symlinkSync(stampedBin, stampedLink);
  if (!binaryMatchesPin(stampedLink, pin)) {
    console.error("FAIL: stamped symlink override was rejected");
    process.exit(1);
  }
  const probeDir = path.join(tmp, "probe-only");
  fs.mkdirSync(probeDir);
  const fake = path.join(probeDir, "fake-workerd");
  fs.writeFileSync(fake, `#!/bin/sh\nprintf '%s\\n' '${pin.release_tag}'\n`);
  fs.chmodSync(fake, 0o755);
  if (!binaryMatchesPin(fake, pin)) {
    console.error("FAIL: --version probe rejected a current absolute binary");
    process.exit(1);
  }
  const fakeLink = path.join(probeDir, "fake-workerd-link");
  fs.symlinkSync(fake, fakeLink);
  if (!binaryMatchesPin(fakeLink, pin)) {
    console.error("FAIL: --version symlink override was rejected");
    process.exit(1);
  }
  console.log("ok binaryMatchesPin follows override symlinks and probes --version");

  const { ensureWorkerd, binaryName } = await import("../dist/sparse-workerd/ensure.js");
  const prevBin = process.env.BOOKCLERK_WORKERD_BIN;
  const overrideCache = path.join(tmp, "ensure-cache");
  fs.mkdirSync(overrideCache);
  process.env.BOOKCLERK_WORKERD_BIN = stampedLink;
  const origFetch = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new Error("ensureWorkerd must not download when override matches");
  };
  try {
    const got = await ensureWorkerd(overrideCache);
    if (fs.realpathSync(got) !== fs.realpathSync(stampedBin)) {
      console.error("FAIL: stamped symlink override did not short-circuit", got);
      process.exit(1);
    }
    process.env.BOOKCLERK_WORKERD_BIN = fakeLink;
    const gotProbe = await ensureWorkerd(overrideCache);
    if (fs.realpathSync(gotProbe) !== fs.realpathSync(fake)) {
      console.error("FAIL: --version symlink override did not short-circuit", gotProbe);
      process.exit(1);
    }
  } finally {
    globalThis.fetch = origFetch;
    if (prevBin === undefined) delete process.env.BOOKCLERK_WORKERD_BIN;
    else process.env.BOOKCLERK_WORKERD_BIN = prevBin;
  }
  // Managed cache leaf symlink still refused before reuse.
  const managed = path.join(overrideCache, binaryName());
  const managedOutside = path.join(tmp, "outside-managed-bin");
  fs.writeFileSync(managedOutside, "OUT");
  try {
    fs.unlinkSync(managed);
  } catch {
    /* missing */
  }
  fs.symlinkSync(managedOutside, managed);
  process.env.BOOKCLERK_WORKERD_BIN = path.join(tmp, "missing-override");
  globalThis.fetch = async () => {
    throw new Error("download must not run after managed symlink refusal");
  };
  try {
    await ensureWorkerd(overrideCache);
    console.error("FAIL: managed cache symlink leaf was accepted");
    process.exit(1);
  } catch (err) {
    const msg = String(err?.message || err);
    if (!/symlink/i.test(msg)) {
      console.error("FAIL: expected managed symlink refusal, got", msg);
      process.exit(1);
    }
  } finally {
    globalThis.fetch = origFetch;
    if (prevBin === undefined) delete process.env.BOOKCLERK_WORKERD_BIN;
    else process.env.BOOKCLERK_WORKERD_BIN = prevBin;
  }
  if (fs.readFileSync(managedOutside, "utf8") !== "OUT") {
    console.error("FAIL: managed symlink refusal mutated outside bytes");
    process.exit(1);
  }
  console.log("ok ensureWorkerd accepts override symlinks; refuses managed leaf links");

  console.log("path security regressions passed");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
