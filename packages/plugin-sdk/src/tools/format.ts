/**
 * Canonical `plugin.toml` emit — field order matches `bookclerk-plugin-manifest`
 * / `toml::to_string_pretty` for conformance fixtures.
 */

import type { EventConsumerToml, Manifest } from "./validate.js";

function esc(s: string): string {
  return JSON.stringify(s);
}

/**
 * Emits a TOML array the way `toml::to_string_pretty` does: `[]` when empty,
 * inline for a single element, one element per line otherwise.
 *
 * @param values - Already-rendered TOML literals.
 * @returns TOML array literal.
 */
function emitArray(values: string[]): string {
  if (values.length === 0) return "[]";
  if (values.length === 1) return `[${values[0]}]`;
  return `[\n${values.map((v) => `    ${v},`).join("\n")}\n]`;
}

function emitStringArray(values: string[]): string {
  return emitArray(values.map((v) => esc(v)));
}

function emitNumberArray(values: number[]): string {
  return emitArray(values.map((v) => String(v)));
}

/**
 * Emits a scalar (or string array) `[vars]` / filter value.
 *
 * @param value - Parsed TOML value.
 * @returns TOML literal, or `null` when the value shape is not emitted.
 */
function emitValue(value: unknown): string | null {
  if (typeof value === "string") return esc(value);
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "number" || typeof value === "bigint") return String(value);
  if (Array.isArray(value) && value.every((v) => typeof v === "string")) {
    return emitStringArray(value as string[]);
  }
  return null;
}

/**
 * Emits `key = value` rows for a flat table, keys sorted like `BTreeMap`.
 *
 * @param lines - Output line buffer.
 * @param table - Parsed TOML table.
 */
function emitTableRows(lines: string[], table: Record<string, unknown>): void {
  for (const key of Object.keys(table).sort()) {
    const rendered = emitValue(table[key]);
    if (rendered !== null) lines.push(`${key} = ${rendered}`);
  }
}

function emitConsumer(lines: string[], consumer: EventConsumerToml): void {
  lines.push("");
  lines.push("[[events.consumers]]");
  lines.push(`type = ${esc(consumer.type)}`);
  lines.push(`schema_versions = ${emitNumberArray(consumer.schema_versions ?? [1])}`);
  lines.push(`supports_suspend = ${consumer.supports_suspend === true ? "true" : "false"}`);
  lines.push(`resource_class = ${esc(consumer.resource_class ?? "network")}`);
  if (consumer.max_retries != null) {
    lines.push(`max_retries = ${consumer.max_retries}`);
  }
  if (consumer.filter && Object.keys(consumer.filter).length > 0) {
    lines.push("");
    lines.push("[events.consumers.filter]");
    emitTableRows(lines, consumer.filter);
  }
}

function emitNamedBinding(
  lines: string[],
  header: string,
  binding: { binding?: string } | undefined,
): void {
  if (!binding) return;
  lines.push("");
  lines.push(header);
  if (binding.binding) lines.push(`binding = ${esc(binding.binding)}`);
}

/**
 * Formats a validated manifest object to canonical TOML.
 *
 * Output is stable across the Rust / TypeScript / Python author tools so
 * `bookclerk-plugin fmt --check` and CI fixtures agree.
 *
 * @param m - Manifest previously accepted by `validateManifest`.
 * @returns Canonical TOML text ending in a trailing newline.
 *
 * @example
 * ```ts
 * const text = formatManifest(validateAndLoad("./plugin.toml"));
 * fs.writeFileSync("plugin.toml", text);
 * ```
 */
export function formatManifest(m: Manifest): string {
  const lines: string[] = [];
  lines.push(`api_version = ${m.api_version}`);
  lines.push(`id = ${esc(m.id)}`);
  if (m.name != null) lines.push(`name = ${esc(m.name)}`);
  if (m.version != null) lines.push(`version = ${esc(m.version)}`);
  if (m.logo != null) lines.push(`logo = ${esc(m.logo)}`);
  const runtime = m.runtime ?? "native";
  lines.push(`runtime = ${esc(runtime)}`);
  if (m.command != null) lines.push(`command = ${esc(m.command)}`);
  if (m.args && m.args.length) {
    lines.push(`args = ${emitStringArray(m.args)}`);
  }
  if (m.entrypoints && m.entrypoints.length) {
    lines.push(`entrypoints = ${emitStringArray(m.entrypoints)}`);
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
    const limits = m.workerd.limits;
    if (limits && (limits.cpu_ms != null || limits.subrequests != null)) {
      lines.push("");
      lines.push("[workerd.limits]");
      if (limits.cpu_ms != null) lines.push(`cpu_ms = ${limits.cpu_ms}`);
      if (limits.subrequests != null) lines.push(`subrequests = ${limits.subrequests}`);
    }
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

  if (m.triggers?.jobs?.length) {
    lines.push("");
    lines.push("[triggers]");
    lines.push(`jobs = ${emitStringArray(m.triggers.jobs)}`);
  }

  for (const consumer of m.events?.consumers ?? []) {
    emitConsumer(lines, consumer);
  }
  for (const producer of m.events?.producers ?? []) {
    lines.push("");
    lines.push("[[events.producers]]");
    lines.push(`type = ${esc(producer.type)}`);
    if (producer.binding) lines.push(`binding = ${esc(producer.binding)}`);
  }

  for (const db of m.databases ?? []) {
    lines.push("");
    lines.push("[[databases]]");
    lines.push(`binding = ${esc(db.binding)}`);
  }

  if (m.vars) {
    lines.push("");
    lines.push("[vars]");
    emitTableRows(lines, m.vars);
  }
  emitNamedBinding(lines, "[secrets]", m.secrets);
  for (const kv of m.kv_namespaces ?? []) {
    emitNamedBinding(lines, "[[kv_namespaces]]", kv);
  }
  emitNamedBinding(lines, "[work_fs]", m.work_fs);
  emitNamedBinding(lines, "[oauth]", m.oauth);

  lines.push("");
  lines.push("[capabilities.network]");
  lines.push(`mode = ${esc(m.capabilities.network.mode)}`);
  if (m.capabilities.network.domains?.length) {
    lines.push(
      `domains = ${emitStringArray(m.capabilities.network.domains)}`,
    );
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
      if (!cmd.args?.length) lines.push("args = []");
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

  for (const client of m.oidc?.clients ?? []) {
    lines.push("");
    lines.push("[[oidc.clients]]");
    lines.push(`client_id = ${esc(String(client.client_id))}`);
    if (client.display_name) {
      lines.push(`display_name = ${esc(String(client.display_name))}`);
    }
    lines.push(`callback_path = ${esc(String(client.callback_path))}`);
    lines.push(`public_client = ${client.public_client === false ? "false" : "true"}`);
    const scopes = Array.isArray(client.default_scopes)
      ? (client.default_scopes as string[])
      : [];
    if (scopes.length) lines.push(`default_scopes = ${emitStringArray(scopes)}`);
    lines.push(
      `issue_refresh_token = ${client.issue_refresh_token === false ? "false" : "true"}`,
    );
    lines.push(`origin_config_key = ${esc(String(client.origin_config_key))}`);
  }

  let out = lines.join("\n");
  if (!out.endsWith("\n")) out += "\n";
  return out;
}
