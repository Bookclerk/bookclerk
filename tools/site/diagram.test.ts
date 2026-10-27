// Tests diagram.ts the way the Worker runs it: workerd has no Node `global`
// unless `nodejs_compat` is on, and elkjs (under beautiful-mermaid) needs one
// while it starts, which diagram.ts provides for the call. Node defines
// `global`, so this file removes it before diagram.ts lays anything out; it
// runs in its own process, as `node --test` runs each file. Run with
// `npm test`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { diagram } from "./diagram.ts";

test("diagrams lay out without a Node global, and leave none behind", () => {
  const scope = globalThis as { global?: unknown };
  delete scope.global;
  assert.ok(!("global" in globalThis));
  const drawn = diagram("flowchart LR\n  A --> B", "Diagram1");
  assert.notEqual(drawn, null);
  assert.match(drawn?.svg ?? "", /^<svg /);
  assert.ok(!("global" in globalThis));
});
