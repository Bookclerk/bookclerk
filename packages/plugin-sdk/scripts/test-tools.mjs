#!/usr/bin/env node
/** Conformance: check / fmt --check against abi fixtures. */
import { spawnSync } from "node:child_process";
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
run(["check", path.join(fixtures, "invalid-module-ts")], false, "not implemented yet");
run(["check", path.join(fixtures, "not-implemented-kv")], true);
run(["check", path.join(fixtures, "not-implemented-queues")], true);
run(["fmt", "--check", path.join(fixtures, "valid-workerd/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "valid-native/plugin.fmt.toml")], true);
console.log("tools conformance passed");
