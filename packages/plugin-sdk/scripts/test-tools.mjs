#!/usr/bin/env node
/** Conformance: check / fmt --check against abi fixtures. */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { WORKERD_PIN_COMPAT_DATE } from "../dist/tools/validate.js";

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
// Rewrite the fixture's date to the day after the pin so the fallback
// boundary tracks every pin bump.
{
  const [y, m, d] = WORKERD_PIN_COMPAT_DATE.split("-").map(Number);
  const newer = new Date(Date.UTC(y, m - 1, d + 1)).toISOString().slice(0, 10);
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "compat-date-future-"));
  try {
    fs.cpSync(path.join(fixtures, "valid-compat-date-future"), dir, { recursive: true });
    const manifestPath = path.join(dir, "plugin.toml");
    const manifest = fs.readFileSync(manifestPath, "utf8");
    const rewritten = manifest.replace(
      /^compatibility_date = ".*"$/m,
      `compatibility_date = "${newer}"`,
    );
    if (!rewritten.includes(newer)) {
      console.error("FAIL valid-compat-date-future has no compatibility_date");
      process.exit(1);
    }
    fs.writeFileSync(manifestPath, rewritten);
    run(["check", dir], true, "Falling back");
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
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
run(["check", path.join(fixtures, "invalid-kv-oauth")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-secret")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-work-fs")], false, "collides");
run(["check", path.join(fixtures, "invalid-kv-oauth-name")], false, "collides");
run(["check", path.join(fixtures, "invalid-producer-database")], false, "collides");
run(["check", path.join(fixtures, "valid-producer-shares-oauth-name")], true);
run(["check", path.join(fixtures, "invalid-undeclared-python")], false, "undeclared Python file");
run(["check", path.join(fixtures, "invalid-module-ts")], false, "not implemented yet");
run(["check", path.join(fixtures, "valid-kv")], true);
run(["check", path.join(fixtures, "invalid-producer-host-event")], false, "emitted by the host");
run(["check", path.join(fixtures, "invalid-kv-namespace")], false, "namespace");
run(["check", path.join(fixtures, "valid-queues")], true);
run(["fmt", "--check", path.join(fixtures, "valid-workerd/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "valid-native/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "fmt-full/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "fmt-full/plugin.toml")], false);
// `fmt` rewrites every manifest section to the Rust formatter's gold.
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "fmt-full-"));
  try {
    const target = path.join(dir, "plugin.toml");
    fs.copyFileSync(path.join(fixtures, "fmt-full/plugin.toml"), target);
    run(["fmt", target], true, "wrote");
    const gold = fs.readFileSync(path.join(fixtures, "fmt-full/plugin.fmt.toml"), "utf8");
    if (fs.readFileSync(target, "utf8") !== gold) {
      throw new Error("FAIL fmt-full: fmt output differs from plugin.fmt.toml");
    }
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

// Shared logo and compatibility-date cases: the Rust and Python checkers
// read the same files and must agree on every result and error message.
{
  const { validateLogo, validateAuthorCompatibilityDate } =
    await import("../dist/tools/validate.js");
  const logoCases = JSON.parse(fs.readFileSync(path.join(fixtures, "logo-cases.json"), "utf8"));
  for (const c of logoCases) {
    let got;
    try {
      const v = validateLogo(c.input);
      got = { [v.kind]: v.value };
    } catch (err) {
      got = { error: err.message };
    }
    const { input, ...want } = c;
    if (JSON.stringify(got) !== JSON.stringify(want)) {
      console.error(`FAIL logo ${JSON.stringify(input)}: got ${JSON.stringify(got)}`);
      process.exit(1);
    }
  }
  console.log(`ok ${logoCases.length} shared logo cases`);
  const dates = JSON.parse(fs.readFileSync(path.join(fixtures, "compat-date-cases.json"), "utf8"));
  for (const [list, ok] of [
    [dates.valid, true],
    [dates.invalid, false],
  ]) {
    for (const d of list) {
      let accepted = true;
      try {
        validateAuthorCompatibilityDate(d);
      } catch {
        accepted = false;
      }
      if (accepted !== ok) {
        console.error(`FAIL compatibility_date ${JSON.stringify(d)}: accepted=${accepted}`);
        process.exit(1);
      }
    }
  }
  console.log("ok shared compatibility dates");
}

// Shared manifest validation cases: each manifest is accepted, or refused with
// the exact message the Rust and Python checkers give.
{
  const { parse } = await import("smol-toml");
  const { validateManifest } = await import("../dist/tools/validate.js");
  const doc = JSON.parse(fs.readFileSync(path.join(fixtures, "manifest-cases.json"), "utf8"));
  for (const c of doc.cases) {
    const text =
      doc.prelude +
      (c.top ?? doc.top) +
      "\n" +
      (c.base_tables ?? doc.base_tables) +
      "\n" +
      (c.tables ?? "");
    let got = null;
    try {
      validateManifest(parse(text));
    } catch (err) {
      got = err.message;
    }
    if (got !== c.error) {
      console.error(`FAIL manifest case ${JSON.stringify(c.name)}: got ${JSON.stringify(got)}`);
      process.exit(1);
    }
  }
  console.log(`ok ${doc.cases.length} shared manifest cases`);
}

// Shared main-module cases: the author-model check gives the Rust and Python
// result and message for every source.
{
  const { checkMainModuleSource } = await import("../dist/tools/check.js");
  const cases = JSON.parse(fs.readFileSync(path.join(fixtures, "main-module-cases.json"), "utf8"));
  for (const c of cases) {
    const main = c.language === "js" ? "index.js" : "main.py";
    let got = null;
    try {
      checkMainModuleSource(main, c.src, c.entrypoints, c.language);
    } catch (err) {
      got = err.message;
    }
    if (got !== c.error) {
      console.error(`FAIL main module case ${JSON.stringify(c.name)}: got ${JSON.stringify(got)}`);
      process.exit(1);
    }
  }
  console.log(`ok ${cases.length} shared main module cases`);
}

const { parse } = await import("smol-toml");
const { materializeConfig, ownUserAgentHostsFor } =
  await import("../dist/sparse-workerd/config.js");
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
const dotDir = moduleLoadKey("./modules", "modules/index.js") === "index.js";
if (
  moduleLoadKey("modules", "./modules/index.js") !== "index.js" ||
  loadKey !== "pkg/echo.wasm" ||
  !dotDir
) {
  console.error(
    "FAIL moduleLoadKey did not normalize dot segments",
    Number(loadKey === "pkg/echo.wasm"),
    Number(dotDir),
  );
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
    consumers: [{ queue: "jobs", max_retries: 5, dead_letter_queue: "jobs-dlq" }],
  },
  capabilities: { network: { mode: "deny" } },
});
const keptQueueParts =
  Number(queuesText.includes("[[queues.producers]]")) +
  Number(queuesText.includes("[[queues.consumers]]")) +
  Number(queuesText.includes('dead_letter_queue = "jobs-dlq"'));
if (keptQueueParts !== 3) {
  console.error("FAIL queues formatter dropped a producer or consumer row", keptQueueParts);
  process.exit(1);
}
for (const triggers of [
  { crons: ["*/30 * * * *", "0 3 * * MON"] },
  { jobs: ["echo.sync"], crons: ["0 0 * * *"] },
]) {
  const roundTrip = parse(
    formatManifest({
      api_version: 3,
      id: "echo",
      runtime: "native",
      command: "./echo",
      triggers,
      capabilities: { network: { mode: "deny" } },
    }),
  );
  if (JSON.stringify(roundTrip.triggers) !== JSON.stringify(triggers)) {
    console.error("FAIL formatter changed [triggers]", JSON.stringify(roundTrip.triggers));
    process.exit(1);
  }
}
console.log("ok formatter keeps cron and job triggers");
const { validateManifest } = await import("../dist/tools/validate.js");
let unquotedDateRejected = false;
try {
  validateManifest(
    parse(`
api_version = 3
id = "echo"
runtime = "workerd"
entrypoints = ["cli"]

[workerd]
compatibility_date = 2026-08-01
main_module = "index.js"

[capabilities.network]
mode = "deny"
`),
  );
} catch (err) {
  unquotedDateRejected =
    err instanceof Error && err.message.includes("compatibility_date is required");
}
if (!unquotedDateRejected) {
  console.error(
    "FAIL unquoted compatibility_date was not rejected as a non-string",
    Number(unquotedDateRejected),
  );
  process.exit(1);
}
const bindingBase = {
  api_version: 3,
  id: "echo",
  runtime: "native",
  command: "./echo",
  entrypoints: ["cli"],
  capabilities: { network: { mode: "deny" } },
};
function bindingMessage(manifest) {
  try {
    validateManifest(manifest);
  } catch (err) {
    return err instanceof Error ? err.message : "";
  }
  return "";
}
// [capabilities.network.own_user_agent]: outbound, 1-32 hosts that are not `*`,
// and a one-line reason.
{
  const own = (mode, table) => ({
    ...bindingBase,
    capabilities: { network: { mode, own_user_agent: table } },
  });
  const ok = { hosts: ["*.libro.fm"], reason: "App API contract." };
  const hosts = Array.from({ length: 33 }, (_, i) => `h${i}.example.com`);
  const cases = [
    [own("outbound", ok), ""],
    [own("deny", ok), 'needs mode = "outbound"'],
    [own("outbound", { hosts: [], reason: "r" }), "between 1 and 32"],
    [own("outbound", { hosts, reason: "r" }), "between 1 and 32"],
    [own("outbound", { hosts: ["*"], reason: "r" }), "is not allowed"],
    [own("outbound", { hosts: ["a%41.example.com"], reason: "r" }), "not a valid hostname"],
    [own("outbound", { hosts: ["a.example.com"], reason: " " }), "one line"],
    [own("outbound", { hosts: ["a.example.com"], reason: "a\nb" }), "one line"],
    [own("outbound", { hosts: ["a.example.com"], reason: "x".repeat(301) }), "one line"],
    [own("outbound", { ...ok, extra: true }), "unknown field `extra`"],
  ];
  for (const [manifest, needle] of cases) {
    const message = bindingMessage(manifest);
    if (needle ? !message.includes(needle) : message !== "") {
      console.error("FAIL own_user_agent", JSON.stringify(manifest), message);
      process.exit(1);
    }
  }
  const asciiHosts = ownUserAgentHostsFor([" *.Bücher.example. ", "API.example.com", "xn--"]);
  if (
    JSON.stringify(asciiHosts) !== JSON.stringify(["*.xn--bcher-kva.example", "api.example.com"])
  ) {
    console.error("FAIL ownUserAgentHostsFor", JSON.stringify(asciiHosts));
    process.exit(1);
  }
  // The A-label check itself, independent of what this Node's URL parser does.
  const { isValidALabel } = await import("../dist/tools/idna.js");
  const aLabels = { "xn--fa-hia": true, "xn--3xa": true, example: true };
  for (const bad of ["xn--", "xn--99999999", "xn--abc-", "xn---"]) aLabels[bad] = false;
  for (const [label, valid] of Object.entries(aLabels)) {
    if (isValidALabel(label) !== valid) {
      console.error("FAIL isValidALabel", label, !valid);
      process.exit(1);
    }
  }
  console.log("ok own_user_agent validation");
}
// [triggers] crons: shape, count and duplicates; a cron-only manifest is valid.
{
  const cronOnly = { ...bindingBase, entrypoints: [] };
  const cases = [
    [{ crons: ["*/30 * * * *", "0  3 * * MON"] }, ""],
    [{ crons: ["* * * *"] }, "needs 5 fields"],
    [{ crons: ["0 0 * * *", " 0 0 * * * "] }, "is duplicated"],
    [{ crons: ["0 0 L * *"] }, ""],
    [{ crons: ["0 0 5#2 * *"] }, "is not valid cron"],
    [{ crons: Array.from({ length: 9 }, (_, i) => `${i} * * * *`) }, "at most 8"],
    [{ crons: ["0 0 * * *".padEnd(129, " ") + "x"] }, "at most 128 bytes"],
  ];
  for (const [triggers, expected] of cases) {
    const message = bindingMessage({ ...cronOnly, triggers });
    const ok = expected === "" ? message === "" : message.includes(expected);
    if (!ok) {
      console.error("FAIL crons", JSON.stringify(triggers), "->", message || "(accepted)");
      process.exit(1);
    }
  }
  for (const registry of ["storefront", "storage", "databaseAdapter"]) {
    const message = bindingMessage({
      ...cronOnly,
      entrypoints: [registry],
      triggers: { crons: ["0 0 * * *"] },
    });
    if (!message.includes("cannot be combined")) {
      console.error("FAIL crons beside", registry, "->", message || "(accepted)");
      process.exit(1);
    }
  }
}
const duplicateKv = bindingMessage({
  ...bindingBase,
  kv_namespaces: [{ binding: "CACHE" }, { binding: "CACHE" }],
});
const reservedKv = bindingMessage({
  ...bindingBase,
  kv_namespaces: [{ binding: "CACHE" }, { binding: "OAUTH" }],
});
const badProducer = bindingMessage({
  ...bindingBase,
  events: {
    producers: [
      { type: "book_ready", binding: "EVENTS" },
      { type: "scan_done", binding: "not-valid" },
    ],
  },
});
const rowIndexed =
  duplicateKv.includes("[kv_namespaces][1] binding collides") &&
  !duplicateKv.includes("CACHE") &&
  reservedKv.includes("[kv_namespaces][1] binding collides") &&
  !reservedKv.includes("OAUTH") &&
  badProducer.includes("[events.producers][1] binding must be") &&
  !badProducer.includes("not-valid");
if (!rowIndexed) {
  console.error("FAIL binding errors omitted the row index", Number(rowIndexed));
  process.exit(1);
}
console.log("ok binding errors name the row index");
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
console.log("ok format keeps queue rows and omits empty module paths");

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
{
  const { healthInterfaces, requireHealthy } = await import("../dist/sparse-workerd/smoke.js");
  const probed = healthInterfaces(["cli", "metadata", "storefront"]);
  if (JSON.stringify(probed) !== JSON.stringify(["ContentSource", "MetadataProvider"])) {
    console.error("FAIL smoke probes every health entrypoint", probed);
    process.exit(1);
  }
  if (healthInterfaces(["cli"]).length !== 0) {
    console.error("FAIL smoke probes no health for a cli-only plugin");
    process.exit(1);
  }
  const ready = { ok: true, detail: "" };
  if (requireHealthy("MetadataProvider", ready) !== ready) {
    console.error("FAIL requireHealthy returns a ready reply");
    process.exit(1);
  }
  for (const [reply, expected] of [
    [{ ok: false, detail: "catalog down" }, "not ready: catalog down"],
    [{ ok: false, detail: "" }, "not ready: no detail"],
  ]) {
    try {
      requireHealthy("MetadataProvider", reply);
      console.error("FAIL requireHealthy accepted a not-ready reply");
      process.exit(1);
    } catch (err) {
      if (!String(err && err.message).includes(expected)) {
        console.error("FAIL requireHealthy message", err);
        process.exit(1);
      }
    }
  }
  console.log("ok smoke probes every health entrypoint and fails when not ready");
}
console.log("tools conformance passed");
