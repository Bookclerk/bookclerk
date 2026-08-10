#!/usr/bin/env node
/** Conformance: check / fmt --check against abi fixtures. */
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../../..");
const cli = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../dist/cli.js");
const fixtures = path.join(root, "crates/bookclerk-plugin-abi/fixtures/tools");

function run(args, expectOk) {
  const r = spawnSync(process.execPath, [cli, ...args], {
    encoding: "utf8",
    cwd: root,
  });
  const ok = r.status === 0;
  if (ok !== expectOk) {
    console.error("FAIL", args, "status", r.status, r.stdout, r.stderr);
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
run(["fmt", "--check", path.join(fixtures, "valid-workerd/plugin.fmt.toml")], true);
run(["fmt", "--check", path.join(fixtures, "valid-native/plugin.fmt.toml")], true);
console.log("tools conformance passed");
