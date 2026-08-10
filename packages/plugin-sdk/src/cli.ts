#!/usr/bin/env node
/**
 * `bookclerk-plugin` — check / fmt / package / smoke (TypeScript SDK tools).
 */

import fs from "node:fs";
import path from "node:path";
import { parse as parseToml } from "smol-toml";
import { checkPlugin, syncEmbed } from "./tools/check.js";
import { formatManifest } from "./tools/format.js";
import { packagePlugin } from "./tools/package.js";
import { validateManifest, type Manifest } from "./tools/validate.js";
import { runSmoke } from "./sparse-workerd/smoke.js";

function usage(): void {
  console.error(`bookclerk-plugin — Bookclerk plugin authoring helpers

Usage:
  bookclerk-plugin check [dir]
  bookclerk-plugin fmt [--check] [plugin.toml]
  bookclerk-plugin sync-embed [dir]
  bookclerk-plugin package --out <dir> [plugin-dir]
  bookclerk-plugin smoke [dir]
`);
}

async function main(argv: string[]): Promise<number> {
  const args = argv.slice(2);
  if (args.length === 0 || args[0] === "-h" || args[0] === "--help") {
    usage();
    return args.length === 0 ? 2 : 0;
  }
  const cmd = args[0]!;
  try {
    switch (cmd) {
      case "check": {
        const dir = path.resolve(args[1] ?? ".");
        console.log(checkPlugin(dir));
        return 0;
      }
      case "fmt": {
        let checkOnly = false;
        let file = "plugin.toml";
        for (const a of args.slice(1)) {
          if (a === "--check") checkOnly = true;
          else if (!a.startsWith("-")) file = a;
          else throw new Error(`unknown fmt flag: ${a}`);
        }
        const text = fs.readFileSync(file, "utf8");
        const m = parseToml(text) as Manifest;
        validateManifest(m);
        const formatted = formatManifest(m);
        if (checkOnly) {
          const norm = (s: string) =>
            s.replace(/\r\n/g, "\n").endsWith("\n")
              ? s.replace(/\r\n/g, "\n")
              : s.replace(/\r\n/g, "\n") + "\n";
          if (norm(text) === norm(formatted)) {
            console.log(`ok ${file}`);
            return 0;
          }
          console.error(`would reformat ${file}`);
          return 1;
        }
        fs.writeFileSync(file, formatted);
        console.log(`wrote ${file}`);
        return 0;
      }
      case "sync-embed": {
        const dir = path.resolve(args[1] ?? ".");
        console.log(syncEmbed(dir));
        return 0;
      }
      case "package": {
        let out: string | undefined;
        let dir = ".";
        for (let i = 1; i < args.length; i++) {
          if (args[i] === "--out") {
            out = args[++i];
          } else if (!args[i]!.startsWith("-")) {
            dir = args[i]!;
          } else {
            throw new Error(`unknown package flag: ${args[i]}`);
          }
        }
        if (!out) throw new Error("package requires --out <dir>");
        const archive = packagePlugin(path.resolve(dir), path.resolve(out));
        console.log(`packed ${archive}`);
        return 0;
      }
      case "smoke": {
        const dir = path.resolve(args[1] ?? ".");
        console.log(await runSmoke(dir));
        return 0;
      }
      default:
        console.error(`unknown command: ${cmd}`);
        usage();
        return 2;
    }
  } catch (err) {
    console.error(`${cmd} failed: ${err instanceof Error ? err.message : err}`);
    return 1;
  }
}

main(process.argv).then((code) => {
  process.exitCode = code;
});
