// Tests diagram.ts the way the Worker runs it: workerd has no Node `global`
// unless `nodejs_compat` is on, and elkjs (under beautiful-mermaid) needs one
// while it starts, which diagram.ts provides for the call. The tests run
// with `nodejs_compat`, which defines `global`, so this file removes it
// before diagram.ts lays anything out; Vitest gives each test file its own
// isolate, so nothing else sees the change. Run with `npm test`.

import assert from "node:assert/strict";
import { test } from "vitest";

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
