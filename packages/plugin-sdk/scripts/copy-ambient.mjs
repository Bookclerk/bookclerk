#!/usr/bin/env node
/** Copy ambient `cloudflare:workers` stub into dist and reference it from entry .d.ts files. */
import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const dist = resolve(here, "../dist");
const src = resolve(here, "../src/cloudflare-workers.d.ts");
const dest = resolve(dist, "cloudflare-workers.d.ts");
mkdirSync(dist, { recursive: true });
copyFileSync(src, dest);

const ref = '/// <reference path="./cloudflare-workers.d.ts" />\n';
for (const name of ["index.d.ts", "bookclerk-plugin.d.ts", "workerd.d.ts"]) {
  const path = resolve(dist, name);
  const text = readFileSync(path, "utf8");
  if (!text.includes('cloudflare-workers.d.ts')) {
    writeFileSync(path, ref + text);
  }
}

console.log(`copied ${dest}`);
