#!/usr/bin/env node
/**
 * ABI drift guard: schema title/apiVersion, core $defs, and method catalog
 * alignment with packages/plugin-sdk/src/generated.ts METHOD_NAMES.
 */
import { readFileSync, existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const schemaPath = resolve(
  here,
  "../../../crates/bookclerk-plugin-abi/schema/abi.json",
);
const generatedPath = resolve(here, "../src/generated.ts");

if (!existsSync(schemaPath)) {
  console.error(`missing ABI schema: ${schemaPath}`);
  process.exit(1);
}

const schema = JSON.parse(readFileSync(schemaPath, "utf8"));
if (schema.title !== "BookclerkPluginAbi") {
  console.error(`unexpected schema title: ${schema.title}`);
  process.exit(1);
}
if (schema.properties?.apiVersion?.const !== 1) {
  console.error("expected properties.apiVersion.const === 1");
  process.exit(1);
}
for (const key of [
  "HandshakeParams",
  "HandshakeResult",
  "HostToPluginEvent",
  "PluginToHostEvent",
  "CliInvokeParams",
  "PluginError",
]) {
  if (!schema.$defs?.[key]) {
    console.error(`schema missing $defs.${key}`);
    process.exit(1);
  }
}

const schemaMethods = Object.keys(schema.properties?.methods?.properties ?? {}).sort();
if (schemaMethods.length === 0) {
  console.error("schema methods.properties is empty");
  process.exit(1);
}

const generatedSrc = readFileSync(generatedPath, "utf8");
const match = generatedSrc.match(
  /export const METHOD_NAMES = \[([\s\S]*?)\] as const/,
);
if (!match) {
  console.error("could not parse METHOD_NAMES from generated.ts");
  process.exit(1);
}
const tsMethods = [...match[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]).sort();

const missingInTs = schemaMethods.filter((m) => !tsMethods.includes(m));
const missingInSchema = tsMethods.filter((m) => !schemaMethods.includes(m));
if (missingInTs.length || missingInSchema.length) {
  if (missingInTs.length) {
    console.error(`methods in schema but not METHOD_NAMES: ${missingInTs.join(", ")}`);
  }
  if (missingInSchema.length) {
    console.error(`methods in METHOD_NAMES but not schema: ${missingInSchema.join(", ")}`);
  }
  process.exit(1);
}

console.log(
  `ok: ${schemaPath} (${schemaMethods.length} methods aligned with generated.ts)`,
);
