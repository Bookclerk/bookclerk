#!/usr/bin/env node
/**
 * Historical native Node entry (SEA / `node src/echo.mjs`).
 *
 * This example now validates workerd (`modules/index.js`, `runtime = "workerd"`).
 * There is no Node Cap'n Proto guest stack. See README.md.
 */
console.error(
  "echo_native_node is a workerd guest (modules/index.js). Native Node stdio is not the product ABI.",
);
process.exit(1);
