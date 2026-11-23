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
run(["check", path.join(fixtures, "valid-outbound-tcp-only")], true);
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

// Formatter corpus: every first-party manifest, formatted by the Rust
// formatter (crates/bookclerk-plugin-manifest/tests/fmt_corpus.rs). `fmt`
// must write the same bytes for each source.
{
  const corpus = path.join(fixtures, "fmt-corpus");
  const index = JSON.parse(fs.readFileSync(path.join(corpus, "index.json"), "utf8"));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "fmt-corpus-"));
  try {
    for (const [name, source] of Object.entries(index)) {
      const target = path.join(dir, "plugin.toml");
      fs.copyFileSync(path.join(root, source), target);
      run(["fmt", target], true, "wrote");
      const gold = fs.readFileSync(path.join(corpus, name), "utf8");
      if (fs.readFileSync(target, "utf8") !== gold) {
        throw new Error(`FAIL fmt-corpus: ${source} formats differently from ${name}`);
      }
      run(["fmt", "--check", path.join(corpus, name)], true);
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

// Table settings: the TS checker refuses the declarations the Rust schema
// validation refuses, with the same messages.
{
  const { parse } = await import("smol-toml");
  const { validateManifest } = await import("../dist/tools/validate.js");
  const { envPropertiesFor } = await import("../dist/tools/types.js");
  const base =
    'api_version = 3\nid = "tables"\nversion = "0.1.0"\nruntime = "workerd"\nentrypoints = ["cli"]\n' +
    '[workerd]\ncompatibility_date = "2026-08-01"\nmain_module = "index.js"\n' +
    '[capabilities.network]\nmode = "deny"\n[vars]\n';
  const table = '[[settings]]\nkey = "mappings"\ntype = "table"\n';
  const column = (key, type, extra = "") =>
    `[[settings.columns]]\nkey = "${key}"\ntype = "${type}"\n${extra}`;
  const cases = [
    [table, "table setting `mappings` must declare `columns`"],
    [
      '[[settings]]\nkey = "name"\ntype = "string"\n' + column("a", "string"),
      "setting `name` declares `columns` but is not a table",
    ],
    [table + column("a", "string") + column("a", "bool"), "column `a` is duplicated"],
    [table + column("a", "secret"), "column `a` must be a string, bool, integer or enum"],
    [table + column("Bad", "string"), "column `Bad` must match"],
    [
      table + column("a", "enum"),
      "table setting `mappings`: enum setting `a` must declare `values`",
    ],
    [
      table + column("a", "string", "default = 1\n"),
      "table setting `mappings` column default: setting `a` must be a string",
    ],
    [
      table.replace('type = "table"\n', 'type = "table"\ndefault = [{ b = "x" }]\n') +
        column("a", "string", "required = true\n") +
        column("b", "string"),
      "default for setting `mappings` row 1: column `a` is required",
    ],
    [
      table.replace('type = "table"\n', 'type = "table"\ndefault = [{ c = "x" }]\n') +
        column("a", "string"),
      "default for setting `mappings` row 1: column `c` is not declared",
    ],
  ];
  for (const [tables, needle] of cases) {
    let got = null;
    try {
      validateManifest(parse(base + tables));
    } catch (err) {
      got = err.message;
    }
    if (!got || !got.startsWith("plugin.toml: [[settings]] ") || !got.includes(needle)) {
      console.error(`FAIL table case ${JSON.stringify(needle)}: got ${JSON.stringify(got)}`);
      process.exit(1);
    }
  }
  const ok = parse(
    base +
      table +
      column("library", "string", "required = true\n") +
      column("depth", "integer") +
      column("mode", "enum", 'values = [{ id = "read" }, { id = "write" }]\ndefault = "read"\n'),
  );
  validateManifest(ok);
  const config = envPropertiesFor(ok).find((p) => p.name === "CONFIG");
  const want =
    'mappings: Array<{ library: string; depth?: number; mode: "read" | "write" }> | undefined;';
  if (!config || !config.type.includes(want)) {
    console.error("FAIL table CONFIG type", config?.type);
    process.exit(1);
  }
  const { formatManifest } = await import("../dist/tools/format.js");
  const canonical = (value) =>
    JSON.stringify(value, (_, v) =>
      v && typeof v === "object" && !Array.isArray(v)
        ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : 1)))
        : v,
    );
  const withDefault = parse(
    base +
      table.replace(
        'type = "table"\n',
        'type = "table"\ndefault = [{ library = "a" }, { mode = "write", library = "b" }]\n',
      ) +
      column("library", "string", 'label = "Library"\nrequired = true\n') +
      column("mode", "enum", 'values = [{ id = "read", label = "Read" }, { id = "write" }]\n'),
  );
  validateManifest(withDefault);
  const formatted = formatManifest(withDefault);
  const reparsed = parse(formatted);
  validateManifest(reparsed);
  // Name the failed check rather than print the manifest text.
  const failed = [
    canonical(reparsed.settings) !== canonical(withDefault.settings) && "settings changed",
    !formatted.includes(
      '[[settings.default]]\nlibrary = "a"\n\n[[settings.default]]\nlibrary = "b"\nmode = "write"\n',
    ) && "default rows not formatted",
    formatManifest(reparsed) !== formatted && "second format differs",
  ].filter(Boolean);
  if (failed.length) {
    console.error(`FAIL table setting format round trip: ${failed.join(", ")}`);
    process.exit(1);
  }
  console.log(`ok ${cases.length + 2} table setting cases`);
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
  // Key order follows the Rust struct (`crons` before `jobs`), so compare
  // the tables key by key: an added or missing key fails, list order counts.
  const got = roundTrip.triggers ?? {};
  const keys = (t) => Object.keys(t).sort().join(",");
  const same =
    keys(got) === keys(triggers) &&
    Object.keys(triggers).every(
      (key) => JSON.stringify(got[key]) === JSON.stringify(triggers[key]),
    );
  if (!same) {
    console.error("FAIL formatter changed [triggers]", JSON.stringify(roundTrip.triggers));
    process.exit(1);
  }
}

// Free-form values and cron text survive `fmt` the way the host writes them.
{
  const { formatManifest: fmtExact } = await import("../dist/tools/format.js");
  const { validateManifest: check } = await import("../dist/tools/validate.js");
  const source = [
    "api_version = 3",
    'id = "echo"',
    'version = "0.1.0"',
    'runtime = "native"',
    'command = "./echo"',
    'entrypoints = ["cli"]',
    "",
    "[triggers]",
    'crons = ["0  3 * * MON"]',
    "",
    "[vars]",
    "release_date = 2026-08-01",
    "sample = [1.0, 1]",
    '"2" = "two"',
    '"10" = "ten"',
    '__proto__ = "marker"',
    "magnitude = 1e21",
    "",
    "[[vars.rows]]",
    "id = 1",
    "",
    "[[vars.rows]]",
    "[vars.rows.meta]",
    "x = 2",
    "",
    "[capabilities.network]",
    'mode = "deny"',
    "",
  ].join("\n");
  check(parse(source));
  const out = fmtExact(parse(source, { integersAsBigInt: true }), { integersAsBigInt: true });
  const back = parse(out, { integersAsBigInt: true });
  const problems = [
    !out.includes('crons = ["0 3 * * MON"]') && "cron whitespace not normalized",
    !out.includes("release_date = 2026-08-01") && "date literal lost",
    !out.includes("sample = [\n    1.0,\n    1,\n]") && "float/integer kind lost",
    !(
      out.indexOf('10 = "ten"') >= 0 &&
      out.indexOf('2 = "two"') >= 0 &&
      out.indexOf('10 = "ten"') < out.indexOf('2 = "two"')
    ) && "keys missing or not in sorted order",
    !out.includes('__proto__ = "marker"') && "__proto__ key dropped",
    !out.includes("magnitude = 1000000000000000000000.0") && "exponent float not plain",
    (back.vars?.rows?.length ?? 0) !== 2 && "array-of-tables element merged",
    back.vars?.rows?.[0]?.id !== 1n && "first row lost id",
    back.vars?.rows?.[1]?.meta?.x !== 2n && "second row lost meta.x",
  ].filter(Boolean);
  if (problems.length) {
    console.error("FAIL fmt free-form round trip:", problems.join("; "), "\n" + out);
    process.exit(1);
  }
}

// Rules the host enforces while decoding (u16 ports, u32 schema versions,
// the credentials and encoding enums) and the queue `SOCKETS` name.
{
  const { validateManifest: check } = await import("../dist/tools/validate.js");
  const { envPropertiesFor } = await import("../dist/tools/types.js");
  const base = {
    api_version: 3,
    id: "echo",
    version: "0.1.0",
    runtime: "native",
    command: "./echo",
    entrypoints: ["cli"],
    capabilities: { network: { mode: "deny" } },
  };
  const refuses = (label, m, needle) => {
    try {
      check(m);
    } catch (err) {
      if (String(err.message).includes(needle)) return;
      console.error(`FAIL ${label}: wrong error`, err.message);
      process.exit(1);
    }
    console.error(`FAIL ${label}: accepted`);
    process.exit(1);
  };
  refuses(
    "credentials enum",
    { ...base, entrypoints: ["storefront"], credentials: "bogus" },
    "`credentials` must be `shareable` or `single_keeper`",
  );
  refuses(
    "tcp port range",
    {
      ...base,
      capabilities: {
        network: { mode: "outbound", tcp: [{ host: "example.com", ports: [65536] }] },
      },
    },
    "ports must be integers from 0 to 65535",
  );
  refuses(
    "schema version range",
    { ...base, events: { consumers: [{ type: "book_added", schema_versions: [-1] }] } },
    "schema_versions entries must be integers",
  );
  refuses(
    "bad cron before the registry rule",
    { ...base, entrypoints: ["storage"], triggers: { crons: ["bad"] } },
    "5 fields",
  );
  const queue = {
    ...base,
    runtime: "workerd",
    workerd: { compatibility_date: "2026-08-01", main_module: "index.js" },
    queues: { producers: [{ binding: "SOCKETS", queue: "jobs" }] },
  };
  check(queue);
  // The host accepts it, but `Env extends BookclerkEnv` types SOCKETS as a
  // SocketsBinding, so `types` asks for another name instead of writing a
  // declaration that does not compile.
  let typesRefused = "";
  try {
    envPropertiesFor(queue);
  } catch (err) {
    typesRefused = String(err.message);
  }
  if (!typesRefused.includes("rename it so the generated Env compiles")) {
    console.error("FAIL types wrote a SOCKETS queue that cannot compile:", typesRefused);
    process.exit(1);
  }
  refuses(
    "cron minute range",
    { ...base, triggers: { crons: ["60 * * * *"] } },
    "minute value 60 is outside 0-59",
  );
  check({ ...base, triggers: { crons: ["59 * * * *", "0 3 * * MON-FRI", "*/15 * * * *"] } });
  refuses(
    "cron decode before the command rule",
    { ...base, command: undefined, triggers: { crons: ["60 * * * *"] } },
    "minute value 60",
  );
  refuses(
    "registry rule before duplicate crons",
    { ...base, entrypoints: ["storage"], triggers: { crons: ["0 3 * * *", "0 3 * * *"] } },
    "cannot be combined",
  );
  refuses(
    "string setting columns",
    { ...base, settings: [{ key: "x", type: "string", columns: "" }] },
    "`columns` must be an array",
  );
  const hook = base;
  refuses(
    "string webhook headers",
    { ...hook, webhooks: [{ name: "inbound", headers: "x", signature: { scheme: "none" } }] },
    "headers must be an array",
  );
  refuses(
    "extra field on an unsigned webhook",
    {
      ...hook,
      webhooks: [{ name: "inbound", signature: { scheme: "none", header: "x-sig" } }],
    },
    "does not apply to",
  );
  refuses(
    "unknown field on a signed webhook",
    {
      ...hook,
      webhooks: [
        {
          name: "inbound",
          signature: {
            scheme: "hmac-sha256",
            header: "x-sig",
            secret_setting: "hook_key",
            algorithm: "sha1",
          },
        },
      ],
    },
    "signature.algorithm is not a field",
  );
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
// [acquire]: the host parses a table with deny_unknown_fields, so a scalar
// or array is refused as well as an unknown key, and an empty table is fine.
{
  const store = (acquire) => ({
    ...bindingBase,
    entrypoints: ["storefront"],
    media: { works: ["book"], lists: ["audio"], fetches: ["audio"] },
    acquire,
  });
  const cases = [
    [{}, ""],
    [{ downloads_per_day: 100 }, ""],
    [false, "plugin.toml: `[acquire]` must be a table"],
    [100, "plugin.toml: `[acquire]` must be a table"],
    [[1], "plugin.toml: `[acquire]` must be a table"],
    [parse("d = 1979-05-27").d, "plugin.toml: `[acquire]` must be a table"],
    [Object.create(null), ""],
    [{ downloads_per_days: 1 }, "unknown field `downloads_per_days`"],
  ];
  for (const [acquire, want] of cases) {
    const got = bindingMessage(store(acquire));
    if (want === "" ? got !== "" : !got.includes(want)) {
      console.error("FAIL [acquire]", JSON.stringify(acquire), JSON.stringify(got));
      process.exit(1);
    }
  }
  console.log("ok [acquire] must be a table of known fields");
}
// [metadata_files]: the host parses rows with deny_unknown_fields and a bool `writes`.
{
  const dialect = { id: "txt", name: "Text", file_names: ["desc.txt"], fields: ["description"] };
  const files = (table) => ({
    ...bindingBase,
    entrypoints: ["metadataFiles"],
    metadata_files: table,
  });
  const cases = [
    [files({ dialects: [dialect] }), ""],
    [files({ dialects: [{ ...dialect, writes: true }] }), ""],
    [
      files({ dialects: [{ ...dialect, wrties: true }] }),
      "plugin.toml: metadata_files.dialects[0] has unknown key `wrties`",
    ],
    [
      files({ dialects: [{ ...dialect, writes: "false" }] }),
      "plugin.toml: metadata_files.dialects[0].writes must be a boolean",
    ],
    [
      files({ dialects: [dialect], extra: 1 }),
      "plugin.toml: `[metadata_files]` has unknown key `extra`",
    ],
  ];
  for (const [manifest, want] of cases) {
    const got = bindingMessage(manifest);
    if (got !== want) {
      console.error("FAIL metadata_files key check", JSON.stringify({ got, want }));
      process.exit(1);
    }
  }
  console.log("ok metadata_files rejects unknown keys and non-bool writes");
}
// [notifier] and [[notifications.types]]: the host parses both with
// deny_unknown_fields and typed enums; the SDK checks reject the same input.
{
  const notifier = (table) => ({ ...bindingBase, entrypoints: ["notifier"], notifier: table });
  const raiser = (types, extra = {}) => ({ ...bindingBase, notifications: { types }, ...extra });
  const type = {
    id: "sync_failed",
    audience: "operational",
    severity: "warning",
    title: "Sync failed",
  };
  const cases = [
    [notifier({ destinations: ["channel"] }), ""],
    [
      notifier({ destinations: ["channel"], extra: 1 }),
      "plugin.toml: `[notifier]` has unknown key `extra`",
    ],
    [
      notifier({ destinations: ["channel"], account_mail: "yes" }),
      "plugin.toml: `[notifier].account_mail` must be a boolean",
    ],
    [
      notifier({ destinations: ["pager"] }),
      "plugin.toml: `[notifier].destinations` names unknown kind `pager`",
    ],
    [
      notifier({ destinations: ["channel"], formats: ["text", "rtf"] }),
      "plugin.toml: `[notifier].formats` names unknown format `rtf`",
    ],
    [
      notifier({
        destinations: ["personal"],
        address_fields: [{ key: "email", label: "Email", required: true, hint: "x" }],
      }),
      "plugin.toml: notifier.address_fields[0] has unknown key `hint`",
    ],
    [raiser([type]), ""],
    [
      raiser([type], { notifications: { types: [type], extra: 1 } }),
      "plugin.toml: `[notifications]` has unknown key `extra`",
    ],
    [
      raiser([{ ...type, colour: "red" }]),
      "plugin.toml: notifications.types[0] has unknown key `colour`",
    ],
    [
      raiser([{ ...type, severity: "page" }]),
      "plugin.toml: notifications.types[0].severity must be `info`, `warning` or `critical`",
    ],
  ];
  for (const [manifest, want] of cases) {
    const got = bindingMessage(manifest);
    if (got !== want) {
      console.error("FAIL notifier key check", JSON.stringify({ got, want }));
      process.exit(1);
    }
  }
  const { renderEnvTypes } = await import("../dist/tools/types.js");
  const types = renderEnvTypes(raiser([type]));
  if (!types.includes("NOTIFY: NotifyBinding;") || !types.includes("NotifyBinding }")) {
    console.error(
      "FAIL notification types env: missing the NOTIFY NotifyBinding property or import",
    );
    process.exit(1);
  }
  if (renderEnvTypes(bindingBase).includes("NOTIFY")) {
    console.error("FAIL a plugin without notification types got a NOTIFY binding");
    process.exit(1);
  }
  console.log("ok notifier and notification types reject unknown keys and type NOTIFY");
}

// [ai]: the host parses it with deny_unknown_fields and a typed task enum;
// the SDK checks reject the same input.
{
  const serving = (table, extra = {}) => ({
    ...bindingBase,
    entrypoints: ["ai"],
    ai: table,
    ...extra,
  });
  const calling = (table) => ({ ...bindingBase, ai: table });
  const cases = [
    [serving({ serves: ["textEmbeddings"] }), ""],
    [calling({ calls: ["textEmbeddings"] }), ""],
    [
      calling({ calls: ["textEmbeddings"], extra: 1 }),
      "plugin.toml: `[ai]` has unknown key `extra`",
    ],
    [serving({ serves: ["speech"] }), "plugin.toml: `[ai].serves` names unknown task `speech`"],
    [calling({ calls: ["speech"] }), "plugin.toml: `[ai].calls` names unknown task `speech`"],
    [
      serving({ serves: ["textEmbeddings"] }, { id: "library" }),
      "plugin.toml: a plugin with the `ai` entrypoint cannot have the id `library`, which names task aliases",
    ],
  ];
  for (const [manifest, want] of cases) {
    const got = bindingMessage(manifest);
    if (got !== want) {
      console.error("FAIL ai key check", JSON.stringify({ got, want }));
      process.exit(1);
    }
  }
  const { renderEnvTypes } = await import("../dist/tools/types.js");
  const types = renderEnvTypes(calling({ calls: ["textEmbeddings"] }));
  if (!types.includes("AI: AiBinding;") || !types.includes("AiBinding")) {
    console.error("FAIL ai calls env: missing the AI AiBinding property or import");
    process.exit(1);
  }
  if (renderEnvTypes(serving({ serves: ["textEmbeddings"] })).includes("AI:")) {
    console.error("FAIL a plugin that calls no model got an AI binding");
    process.exit(1);
  }
  console.log("ok [ai] rejects unknown keys and tasks and types AI");
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
  const asciiHosts = ownUserAgentHostsFor([
    " *.Bücher.example. ",
    "API.example.com",
    "xn--",
    "*.API.123",
  ]);
  if (
    JSON.stringify(asciiHosts) !==
    JSON.stringify(["*.xn--bcher-kva.example", "api.example.com", "*.api.123"])
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
// [acquire]: the host refuses fields it does not know, so the tools do too.
{
  const storefront = (acquire) => ({
    ...bindingBase,
    entrypoints: ["storefront"],
    media: { works: ["book"], lists: ["audio"], fetches: ["audio"] },
    acquire,
  });
  const cases = [
    [storefront({}), ""],
    [storefront({ downloads_per_day: 100, min_gap_seconds: 30 }), ""],
    [storefront({ downloads_per_days: 100 }), "unknown field `downloads_per_days`"],
  ];
  for (const [manifest, needle] of cases) {
    const message = bindingMessage(manifest);
    if (needle ? !message.includes(needle) : message !== "") {
      console.error("FAIL acquire", JSON.stringify(manifest), message);
      process.exit(1);
    }
  }
  console.log("ok acquire rejects unknown keys");
}
// [triggers] crons: shape, count and duplicates; a cron-only manifest is valid.
{
  const cronOnly = { ...bindingBase, entrypoints: [] };
  const cases = [
    [{ crons: ["*/30 * * * *", "0  3 * * MON"] }, ""],
    [{ crons: ["* * * *"] }, "needs 5 fields"],
    [{ crons: ["0 0 * * *", " 0 0 * * * "] }, "is duplicated"],
    [{ crons: ["0 0 L * *"] }, "is not recognized"],
    [{ crons: ["0 0 5#2 * *"] }, "is not recognized"],
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
      ...(registry === "storefront" ? { media: { works: ["book"], lists: ["audio"] } } : {}),
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
// [sockets]: outbound only, a valid name that collides with nothing, and the
// generated Env types it as SocketsBinding under its (renamed) binding.
{
  const { renderEnvTypes } = await import("../dist/tools/types.js");
  const outbound = { ...bindingBase, capabilities: { network: { mode: "outbound", tcp: [] } } };
  const cases = [
    [{ ...bindingBase, sockets: {} }, "[sockets] needs capabilities.network.mode"],
    [{ ...outbound, sockets: {} }, ""],
    [{ ...outbound, sockets: { binding: "push" } }, "[sockets][0] binding must be"],
    [
      { ...outbound, sockets: {}, kv_namespaces: [{ binding: "SOCKETS" }] },
      "[kv_namespaces][0] binding collides",
    ],
    [
      {
        ...outbound,
        sockets: { binding: "EVENTS" },
        events: { producers: [{ type: "echo_done" }] },
      },
      "[sockets][0] binding collides",
    ],
    [
      {
        ...outbound,
        sockets: { binding: "EVENTS" },
        events: { producers: [{ type: "echo_done", binding: "CUSTOM" }] },
      },
      "[sockets][0] binding collides",
    ],
  ];
  for (const [manifest, expected] of cases) {
    const message = bindingMessage(manifest);
    const ok = expected === "" ? message === "" : message.includes(expected);
    if (!ok) {
      console.error(
        "FAIL sockets",
        JSON.stringify(manifest.sockets),
        "->",
        message || "(accepted)",
      );
      process.exit(1);
    }
  }
  const types = renderEnvTypes({ ...outbound, sockets: { binding: "PUSH" } });
  if (!types.includes("PUSH: SocketsBinding;") || !types.includes("SocketsBinding }")) {
    console.error("FAIL sockets env types: missing the PUSH SocketsBinding property or import");
    process.exit(1);
  }
  const formatted = formatManifest({ ...outbound, sockets: { binding: "PUSH" } });
  if (!formatted.includes('[sockets]\nbinding = "PUSH"')) {
    console.error("FAIL sockets format: missing the renamed [sockets] table");
    process.exit(1);
  }
  console.log("ok [sockets] validates, formats and types as SocketsBinding");
  const relay = renderEnvTypes({
    ...outbound,
    entrypoints: ["ingress"],
    ingress: { kind: "relay" },
  });
  if (!relay.includes("ORIGIN: OriginBinding;") || !relay.includes("OriginBinding")) {
    console.error("FAIL ingress env types: a relay plugin lacks the ORIGIN binding");
    process.exit(1);
  }
  const direct = renderEnvTypes({
    ...outbound,
    entrypoints: ["ingress"],
    ingress: { kind: "direct" },
  });
  if (direct.includes("ORIGIN")) {
    console.error("FAIL ingress env types: a direct plugin got the ORIGIN binding");
    process.exit(1);
  }
  console.log("ok a relay [ingress] types ORIGIN as OriginBinding");
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
  const files = healthInterfaces(["metadataFiles", "covers"]);
  if (JSON.stringify(files) !== JSON.stringify(["CoverFetcher", "MetadataFiles"])) {
    console.error("FAIL smoke probes metadataFiles health", files);
    process.exit(1);
  }
  const models = healthInterfaces(["ai"]);
  if (JSON.stringify(models) !== JSON.stringify(["AiModels"])) {
    console.error("FAIL smoke probes ai health", models);
    process.exit(1);
  }
  const notifier = healthInterfaces(["notifier"]);
  if (JSON.stringify(notifier) !== JSON.stringify(["Notifier"])) {
    console.error("FAIL smoke probes notifier health", notifier);
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
