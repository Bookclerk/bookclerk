#!/usr/bin/env node
/** Conformance: check / fmt --check against abi fixtures. */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const cli = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../dist/cli.js");
const fixtures = path.join(root, "crates/bookclerk-plugin-abi/fixtures/tools");

function run(args, expectOk, needle) {
  const r = spawnSync(process.execPath, [cli, ...args], {
    encoding: "utf8",
    cwd: root,
  });
  const ok = r.status === 0;
  const text = `${r.stdout}\n${r.stderr}`;
  if (ok !== expectOk || (needle && !text.includes(needle))) {
    console.error("FAIL", args, "status", r.status, "needle", needle, r.stdout, r.stderr);
    process.exit(1);
  }
  console.log("ok", args.join(" "), "->", r.status);
}

run(["check", path.join(fixtures, "valid-workerd")], true);
run(["check", path.join(fixtures, "valid-logo-url")], true);
run(["check", path.join(fixtures, "valid-logo-path")], true);
run(["check", path.join(fixtures, "invalid-outbound-no-domains")], false);
run(["check", path.join(fixtures, "invalid-native-with-domains")], false);
run(["check", path.join(fixtures, "invalid-logo-javascript")], false);
run(["check", path.join(fixtures, "invalid-logo-vbscript")], false);
run(["check", path.join(fixtures, "invalid-logo-parent")], false);
run(["check", path.join(fixtures, "valid-compat-date-future")], true, "Falling back");
run(["check", path.join(fixtures, "invalid-compat-date-shape")], false, "YYYY-MM-DD");
run(["check", path.join(fixtures, "invalid-compat-flag")], false, "not allowed");
run(["check", path.join(fixtures, "invalid-compat-experimental")], false, "host-only");
run(["check", path.join(fixtures, "invalid-python-flags-missing")], false, "must include");
run(["check", path.join(fixtures, "invalid-flags-without-python")], false, "Python module");
run(["check", path.join(fixtures, "invalid-module-type")], false, "does not match");
run(["check", path.join(fixtures, "invalid-module-path")], false, "missing.js");
run(["check", path.join(fixtures, "invalid-module-path")], false, "not in the workerd load set");
run(["check", path.join(fixtures, "valid-module-path-wins")], true);
run(["check", path.join(fixtures, "valid-js-with-python-helper")], true);
run(["check", path.join(fixtures, "valid-module-name-only")], true);
run(["check", path.join(fixtures, "invalid-kv-oauth")], false, "OAUTH");
run(["check", path.join(fixtures, "invalid-kv-oauth")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-secret")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-work-fs")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-oauth-name")], false, "collides");
run(["check", path.join(fixtures, "invalid-producer-database")], false, "collides");
run(["check", path.join(fixtures, "invalid-undeclared-python")], false, "undeclared Python file");
run(["check", path.join(fixtures, "invalid-module-ts")], false, "not implemented yet");
run(["check", path.join(fixtures, "not-implemented-kv")], true);
run(["check", path.join(fixtures, "not-implemented-queues")], true);
run(["fmt", "--check", path.join(fixtures, "valid-workerd/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "valid-native/plugin.fmt.toml")], true);

const { parse } = await import("smol-toml");
const { materializeConfig } = await import("../dist/sparse-workerd/config.js");
const conflictDir = path.join(fixtures, "invalid-module-path");
const conflictManifest = parse(fs.readFileSync(path.join(conflictDir, "plugin.toml"), "utf8"));
try {
  materializeConfig(conflictDir, conflictManifest, { listenPort: 0, bridgeToken: "token" });
  console.error("FAIL materialize accepted a path that only matches module.name");
  process.exit(1);
} catch (err) {
  const message = String(err && err.message ? err.message : err);
  if (!message.includes("missing.js") || !message.includes("not in the workerd load set")) {
    console.error("FAIL materialize error", message);
    process.exit(1);
  }
}
console.log("ok materialize rejects explicit path when name matches another file");

const { formatManifest } = await import("../dist/tools/format.js");
const { moduleLoadKey } = await import("../dist/tools/validate.js");
const loadKey = moduleLoadKey("modules", "modules/pkg/./echo.wasm");
if (moduleLoadKey("modules", "./modules/index.js") !== "index.js" || loadKey !== "pkg/echo.wasm") {
  console.error("FAIL moduleLoadKey did not normalize dot segments", Number(loadKey === "pkg/echo.wasm"));
  process.exit(1);
}
const queuesText = formatManifest({
  api_version: 3,
  id: "echo",
  runtime: "native",
  command: "./echo",
  entrypoints: ["cli"],
  queues: {
    producers: [{ binding: "MY_QUEUE", queue: "jobs" }],
    names: ["a"],
    empty: [],
  },
  capabilities: { network: { mode: "deny" } },
});
const keptQueueParts =
  Number(queuesText.includes("[[queues.producers]]")) +
  Number(queuesText.includes('names = ["a"]')) +
  Number(queuesText.includes("empty = []"));
if (keptQueueParts !== 3) {
  console.error("FAIL queues formatter dropped scalar arrays", keptQueueParts);
  process.exit(1);
}
const scalarTableCount =
  Number(queuesText.includes("[[queues.names]]")) +
  Number(queuesText.includes("[[queues.empty]]"));
if (scalarTableCount !== 0) {
  console.error("FAIL queues formatter treated a scalar array as tables", scalarTableCount);
  process.exit(1);
}
let nestedRejected = false;
try {
  formatManifest({
    api_version: 3,
    id: "echo",
    runtime: "native",
    command: "./echo",
    entrypoints: ["cli"],
    queues: { meta: { region: "us" } },
    capabilities: { network: { mode: "deny" } },
  });
} catch (err) {
  nestedRejected = String(err && err.message ? err.message : err).includes("nested table");
}
if (!nestedRejected) {
  console.error("FAIL queues formatter dropped a nested table", Number(nestedRejected));
  process.exit(1);
}
const nameOnly = formatManifest({
  api_version: 3,
  id: "echo",
  runtime: "workerd",
  entrypoints: ["cli"],
  workerd: { compatibility_date: "2026-08-01", main_module: "index.js" },
  modules: [{ name: "index.js" }],
  capabilities: { network: { mode: "deny" } },
});
const emittedPath = nameOnly.includes("path =");
if (emittedPath) {
  console.error("FAIL formatter emitted an omitted module path", emittedPath);
  process.exit(1);
}
console.log("ok format keeps scalar queues and omits empty module paths");

const undeclaredDir = path.join(fixtures, "invalid-undeclared-python");
const undeclared = parse(fs.readFileSync(path.join(undeclaredDir, "plugin.toml"), "utf8"));
try {
  materializeConfig(undeclaredDir, undeclared, { listenPort: 0, bridgeToken: "token" });
  console.error("FAIL materialize accepted a disk-only .py");
  process.exit(1);
} catch (err) {
  const message = String(err && err.message ? err.message : err);
  if (!message.includes("undeclared Python file")) {
    console.error("FAIL undeclared python error", message);
    process.exit(1);
  }
}
undeclared.workerd.compatibility_flags = ["python_workers", "disable_python_external_sdk"];
try {
  materializeConfig(undeclaredDir, undeclared, { listenPort: 0, bridgeToken: "token" });
  console.error("FAIL materialize accepted a disk-only .py when both flags are set");
  process.exit(1);
} catch (err) {
  const message = String(err && err.message ? err.message : err);
  if (!message.includes("Python module")) {
    console.error("FAIL flagged disk-only python error", message);
    process.exit(1);
  }
}
console.log("ok materialize rejects disk-only python even with both flags");
console.log("tools conformance passed");
