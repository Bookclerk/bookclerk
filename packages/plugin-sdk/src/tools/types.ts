/**
 * `bookclerk-plugin types` — emits a precise `Env` declaration for one
 * `plugin.toml` (the Bookclerk analogue of `wrangler types`).
 *
 * The generated `bookclerk-configuration.d.ts` names every binding the
 * manifest declares so `class Default extends BookclerkEntrypoint<Env>` gets
 * exact property types instead of the loose {@link BookclerkEnv} index
 * signature.
 */

import fs from "node:fs";
import path from "node:path";
import { parse as parseToml } from "smol-toml";
import { validateManifest, type Manifest } from "./validate.js";

/** Default output filename next to `plugin.toml`. */
export const TYPES_OUTPUT_FILE = "bookclerk-configuration.d.ts";

/** Binding names Bookclerk owns (reject collisions with `[[databases]]`). */
const RESERVED_BINDINGS: ReadonlySet<string> = new Set([
  "CONFIG",
  "SECRETS",
  "EVENTS",
  "WORK_FS",
  "KV",
  "OAUTH",
]);

/** One property of the generated `Env` interface. */
interface EnvProperty {
  /** Binding name on `env`. */
  name: string;
  /** TypeScript type expression. */
  type: string;
  /** Doc line rendered above the property. */
  doc: string;
}

/**
 * Renders a TypeScript literal type for `[vars]` values.
 *
 * Strings, numbers, and booleans become literal types; everything else is
 * widened to `unknown` so the declaration stays honest.
 *
 * @param value - Parsed TOML value.
 * @returns TypeScript type expression.
 */
function varType(value: unknown): string {
  switch (typeof value) {
    case "string":
      return JSON.stringify(value);
    case "number":
      return Number.isFinite(value) ? String(value) : "number";
    case "boolean":
      return String(value);
    default:
      if (Array.isArray(value)) return "unknown[]";
      if (value && typeof value === "object") return "Record<string, unknown>";
      return "unknown";
  }
}

/**
 * Validates and quotes an object key for the emitted interface.
 *
 * @param name - Property name.
 * @returns Bare identifier or a quoted string key.
 */
function propertyKey(name: string): string {
  return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name) ? name : JSON.stringify(name);
}

/**
 * Computes the `Env` properties for a validated manifest.
 *
 * @param m - Validated manifest.
 * @returns Properties in stable emission order.
 * @throws {Error} When a `[[databases]]` binding collides with a reserved name.
 */
export function envPropertiesFor(m: Manifest): EnvProperty[] {
  const props: EnvProperty[] = [];
  const vars = m.vars ?? {};
  const varKeys = Object.keys(vars).sort();
  const configType =
    varKeys.length === 0
      ? "Record<string, unknown>"
      : `{\n${varKeys
          .map((k) => `    ${propertyKey(k)}: ${varType(vars[k])};`)
          .join("\n")}\n    [key: string]: unknown;\n  }`;
  props.push({
    name: "CONFIG",
    type: configType,
    doc: "`[vars]` plus operator settings decoded from the granted config payload.",
  });
  if (m.secrets) {
    props.push({
      name: m.secrets.binding ?? "SECRETS",
      type: "Record<string, string>",
      doc: "`[secrets]` values sealed by the operator (present only when granted).",
    });
  }
  if ((m.events?.producers?.length ?? 0) > 0) {
    props.push({
      name: "EVENTS",
      type: "EventPublisherBinding",
      doc: `\`[[events.producers]]\` outbox publisher (${(m.events!.producers ?? [])
        .map((p) => p.type)
        .join(", ")}).`,
    });
  }
  if (m.work_fs) {
    props.push({
      name: m.work_fs.binding ?? "WORK_FS",
      type: "StorageBinding",
      doc: "`[work_fs]` host-granted object storage.",
    });
  }
  for (const kv of m.kv_namespaces ?? []) {
    props.push({
      name: kv.binding ?? "KV",
      type: "unknown",
      doc: "`[[kv_namespaces]]` store (surface reserved).",
    });
  }
  if (m.oauth) {
    props.push({
      name: m.oauth.binding ?? "OAUTH",
      type: "unknown",
      doc: "`[oauth]` loopback helper (surface reserved).",
    });
  }
  for (const db of m.databases ?? []) {
    if (RESERVED_BINDINGS.has(db.binding)) {
      throw new Error(
        `plugin.toml: [[databases]] binding \`${db.binding}\` collides with a Bookclerk binding`,
      );
    }
    props.push({
      name: db.binding,
      type: "DatabaseBinding",
      doc: "`[[databases]]` plugin-owned database (D1-shaped `prepare`/`batch`/`exec`).",
    });
  }
  const seen = new Set<string>();
  for (const p of props) {
    if (seen.has(p.name)) {
      throw new Error(`plugin.toml: binding \`${p.name}\` is declared twice`);
    }
    seen.add(p.name);
  }
  return props;
}

/**
 * Renders the `bookclerk-configuration.d.ts` text for a validated manifest.
 *
 * @param m - Validated manifest.
 * @returns Declaration file contents (ends with a newline).
 * @throws {Error} When binding names collide.
 *
 * @example
 * ```ts
 * const text = renderEnvTypes(parseToml(fs.readFileSync("plugin.toml", "utf8")));
 * ```
 */
export function renderEnvTypes(m: Manifest): string {
  const props = envPropertiesFor(m);
  const used = new Set(props.map((p) => p.type.split(/[^A-Za-z]/)[0]));
  const imports = [
    "BookclerkEnv",
    ...["DatabaseBinding", "EventPublisherBinding", "StorageBinding"].filter((t) => used.has(t)),
  ];
  const lines: string[] = [
    `// Generated by \`bookclerk-plugin types\` from plugin.toml (id=${m.id}). Do not edit.`,
    "// Regenerate after changing [vars], [secrets], [[events.producers]], [[databases]],",
    "// [work_fs], [[kv_namespaces]], or [oauth].",
  ];
  lines.push(`import type { ${imports.join(", ")} } from "@bookclerk/plugin-sdk/workerd";`);
  lines.push("");
  lines.push("/**");
  lines.push(` * Bindings the host grants to \`${m.id}\`; use as`);
  lines.push(" * `class Default extends BookclerkEntrypoint<Env>`.");
  lines.push(" */");
  lines.push("export interface Env extends BookclerkEnv {");
  for (const p of props) {
    lines.push(`  /** ${p.doc} */`);
    lines.push(`  ${propertyKey(p.name)}: ${p.type};`);
  }
  lines.push("}");
  lines.push("");
  return lines.join("\n");
}

/**
 * Generates `bookclerk-configuration.d.ts` beside a plugin's `plugin.toml`.
 *
 * @param pluginDir - Plugin root containing `plugin.toml`.
 * @param outFile - Optional output path (default `<pluginDir>/bookclerk-configuration.d.ts`).
 * @returns Human-readable summary naming the written file.
 * @throws {Error} When the manifest is invalid or bindings collide.
 */
export function generateTypes(pluginDir: string, outFile?: string): string {
  const tomlPath = path.join(pluginDir, "plugin.toml");
  const m = parseToml(fs.readFileSync(tomlPath, "utf8")) as Manifest;
  validateManifest(m);
  const dest = outFile ?? path.join(pluginDir, TYPES_OUTPUT_FILE);
  fs.writeFileSync(dest, renderEnvTypes(m));
  return `wrote ${dest}`;
}
