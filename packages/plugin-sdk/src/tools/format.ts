/**
 * Canonical `plugin.toml` emit: byte for byte what `bookclerk-plugin-manifest`
 * produces with `toml::to_string_pretty`, so `fmt --check` agrees across the
 * Rust, TypeScript and Python author tools.
 *
 * Formatting runs in two steps. {@link canonicalManifest} rebuilds the
 * manifest the way the Rust `PluginManifest` serializes: fields in struct
 * order, serde defaults filled in, and fields the Rust side skips (empty
 * lists, `None`, defaults it omits) left out. {@link emitToml} then writes
 * that tree the way `toml` pretty-prints: a table's plain values first, then
 * its tables and arrays of tables, arrays of more than one element one per
 * line. The shared corpus under
 * `crates/bookclerk-plugin-abi/fixtures/tools/fmt-corpus/` holds the Rust
 * output for every first-party manifest, and the SDK tests compare against it.
 */

import { TomlDate } from "smol-toml";

import type { Manifest } from "./validate.js";

/**
 * A TOML table in emit order (insertion order is the key order). A `Map`
 * keeps integer-like keys (`"10"`, `"2"`) in the order they were inserted
 * and holds `__proto__` as an ordinary key.
 */
type Table = Map<string, TomlValue>;

/**
 * A free-form TOML float, kept apart from integers so `1.0` is written back
 * as a float. Only free-form values ({@link anyValue}) carry it, and only
 * when the source was parsed with integers as `bigint`.
 */
class TomlFloat {
  /**
   * @param value - The float.
   */
  constructor(readonly value: number) {}
}

/** Any value {@link emitToml} writes. */
type TomlValue = string | number | bigint | boolean | TomlFloat | TomlDate | TomlValue[] | Table;

/** How {@link formatManifest} reads numbers in the parsed manifest. */
export interface FormatOptions {
  /**
   * The manifest was parsed with `integersAsBigInt: true`, so integers are
   * `bigint` and every `number` is a TOML float. Without it, free-form whole
   * numbers are written as integers.
   */
  integersAsBigInt?: boolean;
}

/** A parsed manifest table the canonical step reads from. */
type Source = Record<string, unknown>;

/**
 * Whether `value` is a plain object (a TOML table).
 *
 * @param value - Candidate.
 * @returns `true` for a non-null, non-array object.
 */
function isTable(value: unknown): value is Source {
  return (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    !(value instanceof TomlDate) &&
    !(value instanceof Map)
  );
}

/**
 * The table at `key`, or `undefined`.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The child table.
 */
function tableAt(src: Source, key: string): Source | undefined {
  const value = src[key];
  return isTable(value) ? value : undefined;
}

/**
 * The list at `key`, or an empty list.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The list.
 */
function listAt(src: Source, key: string): unknown[] {
  const value = src[key];
  return Array.isArray(value) ? value : [];
}

/**
 * Builds one canonical table from `(key, value)` pairs, dropping the
 * `undefined` ones (fields the Rust side skips).
 *
 * @param entries - Fields in Rust struct order.
 * @returns The table.
 */
function table(entries: Array<[string, TomlValue | undefined]>): Table {
  const out: Table = new Map();
  for (const [key, value] of entries) {
    if (value !== undefined) out.set(key, value);
  }
  return out;
}

/**
 * A table, or `undefined` when it holds nothing (a struct the Rust side
 * skips when empty).
 *
 * @param t - Table.
 * @returns `t`, or `undefined`.
 */
function nonEmpty(t: Table | undefined): Table | undefined {
  return t && t.size > 0 ? t : undefined;
}

/**
 * A string field, or `undefined` when absent (`Option<String>` skipped on
 * `None`).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The value.
 */
function optString(src: Source, key: string): string | undefined {
  const value = src[key];
  return typeof value === "string" ? value : undefined;
}

/**
 * A string field skipped when empty (`String` with `String::is_empty`).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The value, or `undefined` when empty or absent.
 */
function nonEmptyString(src: Source, key: string): string | undefined {
  const value = optString(src, key);
  return value ? value : undefined;
}

/**
 * A required string field (serialized even when empty).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @param fallback - Serde default when absent.
 * @returns The value.
 */
function str(src: Source, key: string, fallback = ""): string {
  return optString(src, key) ?? fallback;
}

/**
 * A boolean field with its serde default.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @param fallback - Default when absent.
 * @returns The value.
 */
function bool(src: Source, key: string, fallback = false): boolean {
  const value = src[key];
  return typeof value === "boolean" ? value : fallback;
}

/**
 * A boolean field skipped when `false` (`std::ops::Not::not`).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns `true`, or `undefined`.
 */
function trueOnly(src: Source, key: string): boolean | undefined {
  return src[key] === true ? true : undefined;
}

/**
 * An integer field, or `undefined` when absent.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The value.
 */
function optInt(src: Source, key: string): number | bigint | undefined {
  const value = src[key];
  return typeof value === "number" || typeof value === "bigint" ? value : undefined;
}

/**
 * An integer field with its serde default.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @param fallback - Default when absent.
 * @returns The value.
 */
function int(src: Source, key: string, fallback: number): number | bigint {
  return optInt(src, key) ?? fallback;
}

/**
 * An integer field skipped when it equals the default the Rust side omits.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @param omitted - The value that is not serialized.
 * @returns The value, or `undefined`.
 */
function intUnless(src: Source, key: string, omitted: number): number | bigint | undefined {
  const value = optInt(src, key);
  return value === undefined || Number(value) === omitted ? undefined : value;
}

/**
 * A list of strings (serialized even when empty).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The strings.
 */
function strings(src: Source, key: string): string[] {
  return listAt(src, key).map((v) => String(v));
}

/**
 * A list of strings skipped when empty (`Vec::is_empty`).
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The strings, or `undefined`.
 */
function nonEmptyStrings(src: Source, key: string): string[] | undefined {
  const out = strings(src, key);
  return out.length ? out : undefined;
}

/**
 * Cron schedules as the host writes them back: fields split on whitespace
 * and joined with single spaces.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @returns The schedules, or `undefined` when empty.
 */
function cronStrings(src: Source, key: string): string[] | undefined {
  return nonEmptyStrings(src, key)?.map((cron) => cron.trim().split(/\s+/).join(" "));
}

/**
 * A list of canonical tables skipped when empty.
 *
 * @param src - Parent table.
 * @param key - Field name.
 * @param each - Canonical form of one entry.
 * @returns The tables, or `undefined`.
 */
function nonEmptyTables(
  src: Source,
  key: string,
  each: (entry: Source) => Table,
): Table[] | undefined {
  const out = listAt(src, key)
    .filter(isTable)
    .map((entry) => each(entry));
  return out.length ? out : undefined;
}

/**
 * A `serde_json::Value` / `toml::Value` field as the Rust side re-serializes
 * it: object keys sorted (`BTreeMap`), everything else as parsed.
 *
 * @param value - Parsed value.
 * @param floats - Every `number` is a float (integers arrived as `bigint`).
 * @returns The canonical value, or `undefined` for a shape TOML cannot hold.
 */
function anyValue(value: unknown, floats: boolean): TomlValue | undefined {
  if (typeof value === "string" || typeof value === "boolean") return value;
  if (typeof value === "bigint") return value;
  if (typeof value === "number") return floats ? new TomlFloat(value) : value;
  if (value instanceof TomlDate) return value;
  if (Array.isArray(value)) {
    const out: TomlValue[] = [];
    for (const item of value) {
      const inner = anyValue(item, floats);
      if (inner === undefined) return undefined;
      out.push(inner);
    }
    return out;
  }
  if (isTable(value)) {
    const out: Table = new Map();
    for (const key of Object.keys(value).sort()) {
      const inner = anyValue(value[key], floats);
      if (inner !== undefined) out.set(key, inner);
    }
    return out;
  }
  return undefined;
}

/**
 * Canonical `NamedBinding` (`[secrets]`, `[work_fs]`, `[oauth]`, `[sockets]`).
 *
 * @param src - Parsed table.
 * @returns Canonical table.
 */
function namedBinding(src: Source): Table {
  return table([["binding", nonEmptyString(src, "binding")]]);
}

/**
 * Canonical `SettingSpec` (also used for table columns).
 *
 * @param src - Parsed `[[settings]]` entry.
 * @param floats - Numbers in free-form values are floats (see {@link FormatOptions}).
 * @returns Canonical table.
 */
function settingSpec(src: Source, floats: boolean): Table {
  const defaultValue = src.default === undefined ? undefined : anyValue(src.default, floats);
  return table([
    ["key", str(src, "key")],
    ["label", nonEmptyString(src, "label")],
    ["type", str(src, "type")],
    [
      "values",
      nonEmptyTables(src, "values", (v) =>
        table([
          ["id", str(v, "id")],
          ["label", nonEmptyString(v, "label")],
        ]),
      ),
    ],
    ["default", defaultValue],
    ["required", trueOnly(src, "required")],
    ["description", optString(src, "description")],
    ["columns", nonEmptyTables(src, "columns", (c) => settingSpec(c, floats))],
    ["network", optString(src, "network")],
    ["default_port", optInt(src, "default_port")],
  ]);
}

/**
 * Canonical `WebhookManifest`, including its `scheme`-tagged signature.
 *
 * @param src - Parsed `[[webhooks]]` entry.
 * @returns Canonical table.
 */
function webhook(src: Source): Table {
  const sig = tableAt(src, "signature") ?? {};
  const scheme = str(sig, "scheme");
  const signature =
    scheme === "hmac-sha256"
      ? table([
          ["scheme", scheme],
          ["header", str(sig, "header")],
          ["prefix", nonEmptyString(sig, "prefix")],
          ["encoding", str(sig, "encoding", "hex")],
          ["secret_setting", str(sig, "secret_setting")],
        ])
      : table([["scheme", scheme]]);
  return table([
    ["name", str(src, "name")],
    ["max_body_bytes", intUnless(src, "max_body_bytes", 64 * 1024)],
    ["rate_per_minute", intUnless(src, "rate_per_minute", 60)],
    ["retention_secs", intUnless(src, "retention_secs", 86_400)],
    ["headers", nonEmptyStrings(src, "headers")],
    ["signature", signature],
  ]);
}

/**
 * Rebuilds a validated manifest the way the Rust `PluginManifest`
 * serializes it.
 *
 * @param manifest - Manifest previously accepted by `validateManifest`.
 * @param options - How numbers were parsed.
 * @returns The canonical table tree {@link emitToml} writes.
 */
export function canonicalManifest(manifest: Manifest, options: FormatOptions = {}): Table {
  const floats = options.integersAsBigInt === true;
  const m = manifest as unknown as Source;
  const workerd = tableAt(m, "workerd");
  const limits = workerd ? tableAt(workerd, "limits") : undefined;
  const limitsTable = limits
    ? table([
        ["cpu_ms", optInt(limits, "cpu_ms")],
        ["subrequests", optInt(limits, "subrequests")],
      ])
    : undefined;
  const triggers = tableAt(m, "triggers") ?? {};
  const triggersTable = table([
    ["crons", cronStrings(triggers, "crons")],
    ["jobs", nonEmptyStrings(triggers, "jobs")],
  ]);
  const events = tableAt(m, "events") ?? {};
  const eventsTable = table([
    [
      "consumers",
      nonEmptyTables(events, "consumers", (c) =>
        table([
          ["type", str(c, "type")],
          [
            "schema_versions",
            c.schema_versions === undefined ? [1] : (anyValue(c.schema_versions, false) ?? []),
          ],
          ["supports_suspend", bool(c, "supports_suspend")],
          ["resource_class", str(c, "resource_class", "network")],
          ["filter", c.filter === undefined ? undefined : anyValue(c.filter, floats)],
          ["max_retries", optInt(c, "max_retries")],
          ["message_retention_period", optInt(c, "message_retention_period")],
        ]),
      ),
    ],
    [
      "producers",
      nonEmptyTables(events, "producers", (p) =>
        table([
          ["type", str(p, "type")],
          ["binding", nonEmptyString(p, "binding")],
        ]),
      ),
    ],
  ]);
  const vars = tableAt(m, "vars");
  // `vars` is skipped only when absent: an empty `[vars]` is kept.
  const varsTable = vars ? (anyValue(vars, floats) as Table) : undefined;
  const queues = tableAt(m, "queues") ?? {};
  const queuesTable = table([
    [
      "producers",
      nonEmptyTables(queues, "producers", (p) =>
        table([
          ["queue", str(p, "queue")],
          ["binding", str(p, "binding")],
        ]),
      ),
    ],
    [
      "consumers",
      nonEmptyTables(queues, "consumers", (c) =>
        table([
          ["queue", str(c, "queue")],
          ["max_batch_size", int(c, "max_batch_size", 10)],
          ["max_batch_timeout", int(c, "max_batch_timeout", 5)],
          ["max_retries", int(c, "max_retries", 3)],
          ["dead_letter_queue", optString(c, "dead_letter_queue")],
          ["retry_delay", int(c, "retry_delay", 0)],
          ["max_concurrency", optInt(c, "max_concurrency")],
          ["message_retention_period", optInt(c, "message_retention_period")],
        ]),
      ),
    ],
  ]);
  const media = tableAt(m, "media");
  const acquire = tableAt(m, "acquire");
  const metadataFiles = tableAt(m, "metadata_files");
  const notifier = tableAt(m, "notifier");
  const notifications = tableAt(m, "notifications") ?? {};
  const notificationTypes = listAt(notifications, "types").filter(isTable);
  const ingress = tableAt(m, "ingress");
  const provisioning = tableAt(m, "provisioning");
  const ai = tableAt(m, "ai") ?? {};
  const aiTable = table([
    ["serves", nonEmptyStrings(ai, "serves")],
    ["calls", nonEmptyStrings(ai, "calls")],
  ]);
  const network = tableAt(tableAt(m, "capabilities") ?? {}, "network") ?? {};
  const ownUserAgent = tableAt(network, "own_user_agent");
  const cli = tableAt(m, "cli");
  const oidc = tableAt(m, "oidc") ?? {};
  const oidcClients = nonEmptyTables(oidc, "clients", (c) =>
    table([
      ["client_id", str(c, "client_id")],
      ["display_name", nonEmptyString(c, "display_name")],
      ["callback_path", str(c, "callback_path")],
      ["public_client", bool(c, "public_client", true)],
      ["default_scopes", nonEmptyStrings(c, "default_scopes")],
      ["issue_refresh_token", bool(c, "issue_refresh_token", true)],
      ["origin_config_key", str(c, "origin_config_key")],
    ]),
  );

  return table([
    ["api_version", int(m, "api_version", 0)],
    ["id", str(m, "id")],
    ["name", optString(m, "name")],
    ["version", optString(m, "version")],
    ["logo", optString(m, "logo")],
    ["runtime", str(m, "runtime", "native")],
    ["command", optString(m, "command")],
    ["args", nonEmptyStrings(m, "args")],
    ["entrypoints", nonEmptyStrings(m, "entrypoints")],
    [
      "workerd",
      workerd
        ? table([
            ["compatibility_date", str(workerd, "compatibility_date")],
            ["compatibility_flags", nonEmptyStrings(workerd, "compatibility_flags")],
            ["main_module", str(workerd, "main_module")],
            ["modules_dir", str(workerd, "modules_dir", "modules")],
            ["entrypoint", str(workerd, "entrypoint", "default")],
            ["limits", nonEmpty(limitsTable)],
          ])
        : undefined,
    ],
    [
      "modules",
      nonEmptyTables(m, "modules", (mod) =>
        table([
          ["name", str(mod, "name")],
          ["path", nonEmptyString(mod, "path")],
          ["type", str(mod, "type", "js")],
        ]),
      ),
    ],
    ["triggers", nonEmpty(triggersTable)],
    ["events", nonEmpty(eventsTable)],
    ["databases", nonEmptyTables(m, "databases", (d) => table([["binding", str(d, "binding")]]))],
    ["vars", varsTable],
    ["settings", nonEmptyTables(m, "settings", (row) => settingSpec(row, floats))],
    ["secrets", tableAt(m, "secrets") ? namedBinding(tableAt(m, "secrets") ?? {}) : undefined],
    [
      "kv_namespaces",
      nonEmptyTables(m, "kv_namespaces", (kv) =>
        table([
          ["binding", nonEmptyString(kv, "binding")],
          ["namespace", nonEmptyString(kv, "namespace")],
          ["max_value_bytes", intUnless(kv, "max_value_bytes", 64 * 1024)],
          ["max_keys", intUnless(kv, "max_keys", 10_000)],
        ]),
      ),
    ],
    ["queues", nonEmpty(queuesTable)],
    ["work_fs", tableAt(m, "work_fs") ? namedBinding(tableAt(m, "work_fs") ?? {}) : undefined],
    ["oauth", tableAt(m, "oauth") ? namedBinding(tableAt(m, "oauth") ?? {}) : undefined],
    ["sockets", tableAt(m, "sockets") ? namedBinding(tableAt(m, "sockets") ?? {}) : undefined],
    ["webhooks", nonEmptyTables(m, "webhooks", webhook)],
    ["credentials", optString(m, "credentials")],
    [
      "media",
      media
        ? table([
            ["works", strings(media, "works")],
            ["lists", strings(media, "lists")],
            ["fetches", nonEmptyStrings(media, "fetches")],
          ])
        : undefined,
    ],
    [
      "acquire",
      acquire
        ? table([
            ["downloads_per_day", optInt(acquire, "downloads_per_day")],
            ["min_gap_seconds", optInt(acquire, "min_gap_seconds")],
            ["cooldown_seconds", optInt(acquire, "cooldown_seconds")],
          ])
        : undefined,
    ],
    [
      "metadata_files",
      metadataFiles
        ? table([
            [
              "dialects",
              listAt(metadataFiles, "dialects")
                .filter(isTable)
                .map((d) =>
                  table([
                    ["id", str(d, "id")],
                    ["name", str(d, "name")],
                    ["file_names", nonEmptyStrings(d, "file_names")],
                    ["suffixes", nonEmptyStrings(d, "suffixes")],
                    ["fields", strings(d, "fields")],
                    ["credit_roles", nonEmptyStrings(d, "credit_roles")],
                    ["writes", bool(d, "writes")],
                  ]),
                ),
            ],
          ])
        : undefined,
    ],
    [
      "notifier",
      notifier
        ? table([
            ["destinations", strings(notifier, "destinations")],
            ["account_mail", bool(notifier, "account_mail")],
            ["formats", notifier.formats === undefined ? ["text"] : strings(notifier, "formats")],
            [
              "address_fields",
              nonEmptyTables(notifier, "address_fields", (f) =>
                table([
                  ["key", str(f, "key")],
                  ["label", str(f, "label")],
                  ["required", bool(f, "required")],
                  ["secret", bool(f, "secret")],
                ]),
              ),
            ],
          ])
        : undefined,
    ],
    [
      "notifications",
      notificationTypes.length
        ? table([
            [
              "types",
              notificationTypes.map((t) =>
                table([
                  ["id", str(t, "id")],
                  ["audience", str(t, "audience")],
                  ["severity", str(t, "severity", "info")],
                  ["title", str(t, "title")],
                ]),
              ),
            ],
          ])
        : undefined,
    ],
    ["ingress", ingress ? table([["kind", str(ingress, "kind")]]) : undefined],
    [
      "provisioning",
      provisioning
        ? table([
            ["provider", str(provisioning, "provider")],
            ["grants", strings(provisioning, "grants")],
            ["token_url", optString(provisioning, "token_url")],
            ["token_permissions", nonEmptyStrings(provisioning, "token_permissions")],
            ["inputs", nonEmptyStrings(provisioning, "inputs")],
            ["outputs", strings(provisioning, "outputs")],
            ["deprovision", bool(provisioning, "deprovision")],
          ])
        : undefined,
    ],
    ["ai", nonEmpty(aiTable)],
    [
      "capabilities",
      table([
        [
          "network",
          table([
            ["mode", str(network, "mode", "deny")],
            ["domains", nonEmptyStrings(network, "domains")],
            [
              "tcp",
              nonEmptyTables(network, "tcp", (t) =>
                table([
                  ["host", str(t, "host")],
                  ["ports", listAt(t, "ports").map((p) => anyValue(p, false) ?? 0)],
                ]),
              ),
            ],
            [
              "allow_undeclared_public_redirects",
              trueOnly(network, "allow_undeclared_public_redirects"),
            ],
            ["address_cidrs", nonEmptyStrings(network, "address_cidrs")],
            [
              "own_user_agent",
              ownUserAgent
                ? table([
                    ["hosts", strings(ownUserAgent, "hosts")],
                    ["reason", str(ownUserAgent, "reason")],
                  ])
                : undefined,
            ],
          ]),
        ],
      ]),
    ],
    [
      "cli",
      cli
        ? table([
            [
              "commands",
              listAt(cli, "commands")
                .filter(isTable)
                .map((c) =>
                  table([
                    ["name", str(c, "name")],
                    ["about", optString(c, "about")],
                    [
                      "args",
                      listAt(c, "args")
                        .filter(isTable)
                        .map((a) =>
                          table([
                            ["name", str(a, "name")],
                            ["long", optString(a, "long")],
                            ["short", optString(a, "short")],
                            ["kind", str(a, "kind", "string")],
                            ["required", bool(a, "required")],
                            ["default", optString(a, "default")],
                            ["about", optString(a, "about")],
                            ["positional", bool(a, "positional")],
                          ]),
                        ),
                    ],
                  ]),
                ),
            ],
          ])
        : undefined,
    ],
    ["oidc", oidcClients ? table([["clients", oidcClients]]) : undefined],
  ]);
}

/**
 * A TOML basic string (every SDK writes basic strings).
 *
 * @param s - Text.
 * @returns Quoted, escaped literal.
 */
function basicString(s: string): string {
  let out = '"';
  for (const ch of s) {
    const code = ch.codePointAt(0) ?? 0;
    if (ch === '"') out += '\\"';
    else if (ch === "\\") out += "\\\\";
    else if (ch === "\n") out += "\\n";
    else if (ch === "\t") out += "\\t";
    else if (ch === "\r") out += "\\r";
    else if (ch === "\b") out += "\\b";
    else if (ch === "\f") out += "\\f";
    else if (code < 0x20 || code === 0x7f)
      out += `\\u${code.toString(16).toUpperCase().padStart(4, "0")}`;
    else out += ch;
  }
  return `${out}"`;
}

/**
 * A float as Rust's `toml` writes it (always with a fraction or exponent).
 *
 * @param n - Value.
 * @returns TOML float literal.
 */
function float(n: number): string {
  if (Number.isNaN(n)) return "nan";
  if (!Number.isFinite(n)) return n > 0 ? "inf" : "-inf";
  if (Number.isInteger(n)) return `${n}.0`;
  return String(n);
}

/**
 * A TOML key, bare when it can be.
 *
 * @param key - Key text.
 * @returns Bare or quoted key.
 */
function emitKey(key: string): string {
  return /^[A-Za-z0-9_-]+$/.test(key) ? key : basicString(key);
}

/**
 * Whether `value` must be written as a table or an array of tables rather
 * than as a `key = value` line.
 *
 * @param value - Field value.
 * @returns `true` for a table or a non-empty array of tables.
 */
function isSection(value: TomlValue): boolean {
  if (value instanceof Map) return true;
  return Array.isArray(value) && value.length > 0 && value.every((item) => item instanceof Map);
}

/**
 * One value in `key = value` position.
 *
 * @param value - Value (never a section).
 * @param inline - Inside an inline array or table: arrays stay on one line.
 * @returns TOML literal.
 */
function emitInline(value: TomlValue, inline = false): string {
  if (typeof value === "string") return basicString(value);
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "bigint") return value.toString();
  if (value instanceof TomlFloat) return float(value.value);
  if (value instanceof TomlDate) return value.toISOString();
  if (typeof value === "number") {
    // The parser hands integers and floats over as numbers; the manifest's
    // numeric fields are integers, so whole numbers are written as such.
    return Number.isInteger(value) ? String(value) : float(value);
  }
  if (Array.isArray(value)) {
    const items = value.map((item) => emitInline(item, true));
    if (items.length === 0) return "[]";
    if (inline) return `[${items.join(", ")}]`;
    if (items.length === 1) return `[${items[0]}]`;
    return `[\n${items.map((item) => `    ${item},`).join("\n")}\n]`;
  }
  const cells = [...value.entries()].map(([k, v]) => `${emitKey(k)} = ${emitInline(v, true)}`);
  return cells.length ? `{ ${cells.join(", ")} }` : "{}";
}

/**
 * Writes one table body (its values, then its sections) into `out`.
 *
 * @param out - Output blocks; each is one section's lines.
 * @param path - Dotted key path of this table (empty for the root).
 * @param body - The table.
 * @param header - Header line, or `null` for the root.
 */
function emitTable(out: string[][], path: string[], body: Table, header: string | null): void {
  const lines: string[] = [];
  const sections: Array<[string, TomlValue]> = [];
  for (const [key, value] of body.entries()) {
    if (isSection(value)) sections.push([key, value]);
    else lines.push(`${emitKey(key)} = ${emitInline(value)}`);
  }
  // `toml` leaves a table implicit when it holds only sections.
  if (header === null) out.push(lines);
  // An array-of-tables element always needs its own `[[...]]` header, or
  // its child sections would attach to the previous element.
  else if (header.startsWith("[[") || lines.length > 0 || sections.length === 0) {
    out.push([header, ...lines]);
  }
  for (const [key, value] of sections) {
    const child = [...path, emitKey(key)];
    if (Array.isArray(value)) {
      for (const item of value) emitTable(out, child, item as Table, `[[${child.join(".")}]]`);
    } else {
      emitTable(out, child, value as Table, `[${child.join(".")}]`);
    }
  }
}

/**
 * Writes a canonical table tree the way `toml::to_string_pretty` does.
 *
 * @param root - Document root.
 * @returns TOML text ending in a newline.
 */
export function emitToml(root: Table): string {
  const blocks: string[][] = [];
  emitTable(blocks, [], root, null);
  const text = blocks
    .filter((block) => block.length > 0)
    .map((block) => block.join("\n"))
    .join("\n\n");
  return text.endsWith("\n") ? text : `${text}\n`;
}

/**
 * Formats a validated manifest to canonical TOML.
 *
 * Output is stable across the Rust / TypeScript / Python author tools so
 * `bookclerk-plugin fmt --check` and CI fixtures agree. Comments are not
 * kept, as with the Rust formatter.
 *
 * @param m - Manifest previously accepted by `validateManifest`.
 * @param options - How numbers were parsed; pass `{ integersAsBigInt: true }`
 *   when the manifest came from `parse(text, { integersAsBigInt: true })` so
 *   free-form floats such as `1.0` stay floats.
 * @returns Canonical TOML text ending in a trailing newline.
 *
 * @example
 * ```ts
 * const text = formatManifest(validateAndLoad("./plugin.toml"));
 * fs.writeFileSync("plugin.toml", text);
 * ```
 */
export function formatManifest(m: Manifest, options: FormatOptions = {}): string {
  return emitToml(canonicalManifest(m, options));
}
