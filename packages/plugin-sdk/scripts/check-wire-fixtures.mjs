#!/usr/bin/env node
/**
 * Load ABI wire golden fixtures and assert camelCase object keys (#130).
 * Run: node scripts/check-wire-fixtures.mjs (from packages/plugin-sdk)
 *   or: node packages/plugin-sdk/scripts/check-wire-fixtures.mjs (from repo root)
 */
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const wireDir = join(here, "../../../crates/bookclerk-plugin-abi/fixtures/wire");

/** Keep in sync with scripts/gen-plugin-abi.py REQUIRED_WIRE_FIXTURES. */
const REQUIRED_WIRE_FIXTURES = [
  "login.request.json",
  "login.result.json",
  "scan.request.json",
  "scan.result.json",
  "fetchTitle.request.json",
  "put.s3.request.json",
];

function collectSnakeKeys(value, path = "$") {
  const bad = [];
  if (value && typeof value === "object" && !Array.isArray(value)) {
    for (const [key, child] of Object.entries(value)) {
      const herePath = `${path}.${key}`;
      if (key.includes("_")) bad.push(herePath);
      bad.push(...collectSnakeKeys(child, herePath));
    }
  } else if (Array.isArray(value)) {
    value.forEach((child, i) => {
      bad.push(...collectSnakeKeys(child, `${path}[${i}]`));
    });
  }
  return bad;
}

const files = readdirSync(wireDir).filter((f) => f.endsWith(".json"));
if (files.length === 0) {
  console.error(`no fixtures in ${wireDir}`);
  process.exit(1);
}

const present = new Set(files);
let failed = false;
for (const name of REQUIRED_WIRE_FIXTURES) {
  if (!present.has(name)) {
    console.error(`missing required wire fixture: ${name}`);
    failed = true;
  }
}

for (const name of files.sort()) {
  const data = JSON.parse(readFileSync(join(wireDir, name), "utf8"));
  const bad = collectSnakeKeys(data);
  if (bad.length) {
    console.error(`${name}: non-camelCase keys: ${bad.join(", ")}`);
    failed = true;
  }
}

if (failed) process.exit(1);
console.log(`ok wire fixtures=${REQUIRED_WIRE_FIXTURES.length} (camelCase keys)`);
