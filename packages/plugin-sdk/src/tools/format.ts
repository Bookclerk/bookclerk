/**
 * Canonical plugin.toml emit — field order matches bookclerk-plugin-manifest /
 * `toml::to_string_pretty` for conformance fixtures.
 */

import type { Manifest } from "./validate.js";

function esc(s: string): string {
  return JSON.stringify(s);
}

function emitStringArray(values: string[], indent = ""): string {
  if (values.length === 0) return "[]";
  const lines = values.map((v) => `${indent}    ${esc(v)},`);
  return `[\n${lines.join("\n")}\n${indent}]`;
}

/** Format a validated manifest object to canonical TOML. */
export function formatManifest(m: Manifest): string {
  const lines: string[] = [];
  lines.push(`api_version = ${m.api_version}`);
  lines.push(`id = ${esc(m.id)}`);
  if (m.name != null) lines.push(`name = ${esc(m.name)}`);
  lines.push(`kind = ${esc(m.kind)}`);
  if (m.version != null) lines.push(`version = ${esc(m.version)}`);
  if (m.logo != null) lines.push(`logo = ${esc(m.logo)}`);
  const runtime = m.runtime ?? "native";
  lines.push(`runtime = ${esc(runtime)}`);
  if (m.command != null) lines.push(`command = ${esc(m.command)}`);
  if (m.args && m.args.length) {
    lines.push(`args = ${emitStringArray(m.args)}`);
  }

  if (m.workerd) {
    lines.push("");
    lines.push("[workerd]");
    lines.push(`compatibility_date = ${esc(m.workerd.compatibility_date)}`);
    if (m.workerd.compatibility_flags?.length) {
      lines.push(
        `compatibility_flags = ${emitStringArray(m.workerd.compatibility_flags)}`,
      );
    }
    lines.push(`main_module = ${esc(m.workerd.main_module)}`);
    const modulesDir = m.workerd.modules_dir ?? "modules";
    lines.push(`modules_dir = ${esc(modulesDir)}`);
    const entrypoint = m.workerd.entrypoint ?? "default";
    lines.push(`entrypoint = ${esc(entrypoint)}`);
  }

  if (m.modules?.length) {
    for (const mod of m.modules) {
      lines.push("");
      lines.push("[[modules]]");
      lines.push(`name = ${esc(mod.name)}`);
      lines.push(`path = ${esc(mod.path)}`);
      lines.push(`type = ${esc(mod.type ?? "js")}`);
    }
  }

  lines.push("");
  lines.push("[capabilities.network]");
  lines.push(`mode = ${esc(m.capabilities.network.mode)}`);
  if (m.capabilities.network.domains?.length) {
    lines.push(
      `domains = ${emitStringArray(m.capabilities.network.domains)}`,
    );
  }

  const b = m.capabilities.bindings ?? {};
  const bindingKeys = ["config", "secrets", "plugin_kv", "work_fs", "oauth"] as const;
  const activeBindings = bindingKeys.filter((k) => b[k]);
  if (activeBindings.length) {
    lines.push("");
    lines.push("[capabilities.bindings]");
    for (const k of activeBindings) {
      lines.push(`${k} = true`);
    }
  }

  const methods = m.capabilities.methods?.list ?? [];
  if (methods.length) {
    lines.push("");
    lines.push("[capabilities.methods]");
    lines.push(`list = ${emitStringArray(methods)}`);
  }

  const cli = m.cli as
    | {
        commands?: Array<{
          name: string;
          about?: string;
          args?: Array<Record<string, unknown>>;
        }>;
      }
    | undefined;
  if (cli?.commands?.length) {
    // Match `toml::to_string_pretty`: array-of-tables without a bare `[cli]`.
    for (const cmd of cli.commands) {
      lines.push("");
      lines.push("[[cli.commands]]");
      lines.push(`name = ${esc(cmd.name)}`);
      if (cmd.about != null) lines.push(`about = ${esc(cmd.about)}`);
      for (const arg of cmd.args ?? []) {
        lines.push("");
        lines.push("[[cli.commands.args]]");
        lines.push(`name = ${esc(String(arg.name))}`);
        if (arg.long != null) lines.push(`long = ${esc(String(arg.long))}`);
        if (arg.short != null) lines.push(`short = ${esc(String(arg.short))}`);
        lines.push(`kind = ${esc(String(arg.kind ?? "string"))}`);
        lines.push(`required = ${arg.required === true ? "true" : "false"}`);
        if (arg.default != null) lines.push(`default = ${esc(String(arg.default))}`);
        if (arg.about != null) lines.push(`about = ${esc(String(arg.about))}`);
        lines.push(`positional = ${arg.positional === true ? "true" : "false"}`);
      }
    }
  }

  let out = lines.join("\n");
  if (!out.endsWith("\n")) out += "\n";
  return out;
}
