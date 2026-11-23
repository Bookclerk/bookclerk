/**
 * `plugin.toml` semantic validation (mirrors `bookclerk-plugin-manifest` rules).
 *
 * Used by `bookclerk-plugin check|fmt|package` and the sparse-workerd smoke path.
 */

import { MAX_CRON_BYTES } from "../abi.js";
import { isValidALabel } from "./idna.js";

/**
 * Parsed `plugin.toml` shape accepted by the TypeScript author tools.
 *
 * Field names match the TOML keys (snake_case), not the Workers RPC wire
 * (camelCase). See `docs/plugins/README.md` for the full manifest contract.
 */
export type Manifest = {
  /** ABI version declared in `plugin.toml`; must be `3`. */
  api_version: number;
  /** Globally unique plugin id matching `[a-z0-9_]{2,32}` (no leading/trailing `_`). */
  id: string;
  /** Optional operator-facing display name when it differs from `id`. */
  name?: string;
  /** Semver-ish package version embedded in release archive filenames. */
  version?: string;
  /** Remote `http(s)` URL or relative image path under the plugin root for the brand logo. */
  logo?: string;
  /** Guest runtime; defaults to `native` when omitted (`workerd` for Workers RPC isolates). */
  runtime?: string;
  /** Native executable path relative to the plugin root (required when `runtime = "native"`). */
  command?: string;
  /** Extra argv appended after `command` when spawning the native guest. */
  args?: string[];
  /**
   * Named entrypoints the guest exports (`storefront`, `storage`,
   * `databaseAdapter`, `remoteLibrary`, `cli`, `oidc`, `metadata`, `covers`,
   * `metadataFiles`, `notifier`, `ai`). Triggers for the
   * default entrypoint live under `[triggers]` / `[[events.consumers]]`.
   */
  entrypoints?: string[];
  /**
   * `[media]` — the work kinds and edition carriers a storefront or metadata
   * provider works with. Required for those plugins, forbidden for others.
   */
  media?: {
    /** Work kinds the plugin reports (`book`, `podcast`). */
    works?: string[];
    /** Carriers it reports in scans, catalog search and metadata (`audio`, `ebook`, `print`). */
    lists?: string[];
    /** Carriers `fetchTitle` can deliver; a subset of `lists`, storefronts only. */
    fetches?: string[];
  };
  /**
   * `[acquire]` — how fast the host may download from one store account when
   * it starts the downloads itself. Optional, storefronts only.
   */
  acquire?: {
    /** Downloads per store account in any rolling 24 hours (1 to 10000). */
    downloads_per_day?: number;
    /** Seconds between two downloads on one store account (0 to 86400). */
    min_gap_seconds?: number;
    /** Seconds to pause after `rateLimited` without a retry time (1 to 604800). */
    cooldown_seconds?: number;
  };
  /**
   * `[metadata_files]` — the metadata file dialects a `metadataFiles` plugin
   * implements. Required for that entrypoint, forbidden for others.
   */
  metadata_files?: {
    /** One row per dialect; never empty. */
    dialects?: MetadataFileDialectToml[];
  };
  /**
   * `[notifier]` — what a `notifier` plugin carries. Required for that
   * entrypoint, forbidden for others.
   */
  notifier?: NotifierToml;
  /** `[notifications]` — notification types raised through `env.NOTIFY`. */
  notifications?: {
    /** One row per type (`[[notifications.types]]`). */
    types?: NotificationTypeToml[];
  };
  /**
   * `[ingress]` — how an `ingress` plugin serves the library publicly.
   * Required for that entrypoint, forbidden for others.
   */
  ingress?: {
    /**
     * `relay`: the provider relays each request to the plugin, which hands it
     * to the library through `env.ORIGIN`. `direct`: the public address
     * points at the daemon; the plugin only keeps it current.
     */
    kind?: string;
  };
  /**
   * `[provisioning]` — the guided setup a `provisioning` plugin offers.
   * Required for that entrypoint, forbidden for others.
   */
  provisioning?: {
    /** The provider the grant is for (`cloudflare`). */
    provider?: string;
    /** Grant kinds the plugin accepts (`api_token`); never empty. */
    grants?: string[];
    /** Where the operator creates a token at the provider (`https://`). */
    token_url?: string;
    /** The permissions a pasted token needs, one line each. */
    token_permissions?: string[];
    /** Non-secret setting keys the setup screen asks for before running. */
    inputs?: string[];
    /** Setting keys, secret or not, the plugin may write; never empty. */
    outputs?: string[];
    /** Whether the plugin can remove what it created. */
    deprovision?: boolean;
  };
  /**
   * `[ai]` — model tasks the plugin serves on its `ai` entrypoint and the
   * tasks it calls through `env.AI`.
   */
  ai?: {
    /** Tasks the plugin's models perform; required with the `ai` entrypoint. */
    serves?: string[];
    /** Tasks the plugin runs through `env.AI`; declaring any asks for the binding. */
    calls?: string[];
  };
  /** Workerd isolate settings (required when `runtime = "workerd"`). */
  workerd?: {
    /** Cloudflare Workers compatibility date (`YYYY-MM-DD`) for the isolate. */
    compatibility_date: string;
    /**
     * Author compatibility flags. Only `python_workers` and
     * `disable_python_external_sdk`, and only together when the guest is Python.
     */
    compatibility_flags?: string[];
    /** Entrypoint module filename under `modules_dir` (for example `plugin.js`). */
    main_module: string;
    /** Modules directory relative to the plugin root (default `modules`). */
    modules_dir?: string;
    /** Named WorkerEntrypoint export on the main module (default `default`). */
    entrypoint?: string;
    /** Optional CPU / subrequest limits enforced by the host egress policy. */
    limits?: { cpu_ms?: number; subrequests?: number };
  };
  /**
   * Extra module descriptors. `path` is the source file. When `path` is
   * omitted or empty, `name` is the source in the modules directory.
   */
  modules?: Array<{ name: string; path?: string; type?: string }>;
  /** `[triggers]` — `jobs` command types for `job(controller)` and `crons` schedules for `scheduled(controller)`. */
  triggers?: { jobs?: string[]; crons?: string[] };
  /** `[events]` — consumer triggers and producer publish grants. */
  events?: {
    /** `[[events.consumers]]` rows delivered to `event(batch)`. */
    consumers?: EventConsumerToml[];
    /** `[[events.producers]]` rows the guest may publish through `EVENTS`. */
    producers?: Array<{ type: string; binding?: string }>;
  };
  /** `[[databases]]` — plugin-owned database bindings on `env`. */
  databases?: Array<{ binding: string }>;
  /** `[vars]` — declares the `CONFIG` binding (values come from `[[settings]]`). */
  vars?: Record<string, unknown>;
  /** `[[settings]]` — operator settings schema validated and rendered by the host. */
  settings?: SettingToml[];
  /** `[secrets]` — sealed operator secrets binding (default name `SECRETS`). */
  secrets?: { binding?: string };
  /**
   * `[[kv_namespaces]]` — durable KV bindings (default name `KV`) served
   * from the library database; distinct from `databases`. `namespace`
   * defaults to the binding name lowercased.
   */
  kv_namespaces?: Array<{
    binding?: string;
    namespace?: string;
    max_value_bytes?: number;
    max_keys?: number;
  }>;
  /**
   * `[queues]` — Workers Queues: producers are `env` send bindings,
   * consumers are `queue(batch)` triggers on the host outbox.
   */
  queues?: QueuesToml;
  /** `[work_fs]` — scratch filesystem binding (default name `WORK_FS`). */
  work_fs?: { binding?: string };
  /** `[oauth]` — loopback OAuth callback binding (default name `OAUTH`). */
  oauth?: { binding?: string };
  /**
   * `[sockets]` — host-brokered WebSockets (default name `SOCKETS`) that stay
   * open while the plugin sleeps; needs `capabilities.network.mode =
   * "outbound"`.
   */
  sockets?: { binding?: string };
  /** Network policy the operator must consent to. */
  capabilities: {
    /** Egress policy (`deny` / `outbound`, plus hostname domains for workerd). */
    network: {
      mode: string;
      domains?: string[];
      /**
       * Hosts that receive the plugin's own `User-Agent` instead of the
       * host's `bookclerk/<version>` agent, with the reason the consent form
       * shows the operator.
       */
      own_user_agent?: { hosts: string[]; reason: string };
    };
  };
  /** Optional CLI schema block (`[[cli.commands]]`) mirrored into describe metadata. */
  cli?: unknown;
  /** Optional `[[oidc.clients]]` relying-party templates. */
  oidc?: { clients?: Array<Record<string, unknown>> };
};

/** One `[[events.consumers]]` row. */
export type EventConsumerToml = {
  /** Versioned event type (`book_acquired`, …). */
  type: string;
  /** Schema versions the guest can consume (default `[1]`). */
  schema_versions?: number[];
  /** Whether the guest supports `suspended` results for this type. */
  supports_suspend?: boolean;
  /** Concurrency class copied onto deliveries (default `network`). */
  resource_class?: string;
  /** Optional host-owned payload filter (top-level key equality). */
  filter?: Record<string, unknown>;
  /** Redelivery attempts before dead-lettering. */
  max_retries?: number;
  /** Seconds a due delivery may wait before the host expires it (60..=1209600). */
  message_retention_period?: number;
};

/** `[queues]` producers and consumers. */
export type QueuesToml = {
  /** `[[queues.producers]]` rows: queue plus the `env` binding that sends to it. */
  producers?: QueueProducerToml[];
  /** `[[queues.consumers]]` rows delivered to `queue(batch)`. */
  consumers?: QueueConsumerToml[];
};

/** One `[[queues.producers]]` row. */
export type QueueProducerToml = {
  /** Queue name (`[a-z0-9][a-z0-9._-]*`; `core.` and `node.` are reserved). */
  queue: string;
  /** Binding name on `env` (`[A-Z][A-Z0-9_]*`); required. */
  binding: string;
};

/** One `[[queues.consumers]]` row. */
export type QueueConsumerToml = {
  /** Queue name to consume. */
  queue: string;
  /** Most messages per `queue(batch)` call (default 10, at most 100). */
  max_batch_size?: number;
  /** Longest wait in seconds to fill a batch (default 5, at most 60). */
  max_batch_timeout?: number;
  /** Redeliveries before dead-lettering (default 3, at most 100). */
  max_retries?: number;
  /** Queue that receives messages once `max_retries` is exhausted. */
  dead_letter_queue?: string;
  /** Seconds a retried message waits before redelivery (default 0, at most 43200). */
  retry_delay?: number;
  /** Most concurrent `queue(batch)` invocations; omitted lets the host choose. */
  max_concurrency?: number;
  /** Seconds a due message may wait before the host expires it (60..=1209600). */
  message_retention_period?: number;
};

/** Shortest `message_retention_period` in seconds, as in Cloudflare Queues. */
export const MIN_MESSAGE_RETENTION_SECS = 60;
/** Longest `message_retention_period` in seconds (14 days), as in Cloudflare Queues. */
export const MAX_MESSAGE_RETENTION_SECS = 1_209_600;

/**
 * Checks a consumer's optional `message_retention_period`.
 *
 * @param field - The consumer row, such as `events.consumers[0]`, for the error.
 * @param secs - The declared retention in seconds; omitted passes.
 * @throws {Error} When `secs` is not an integer inside the allowed range.
 */
function validateMessageRetention(field: string, secs: number | undefined): void {
  if (secs == null) return;
  if (
    !Number.isInteger(secs) ||
    secs < MIN_MESSAGE_RETENTION_SECS ||
    secs > MAX_MESSAGE_RETENTION_SECS
  ) {
    throw new Error(
      `plugin.toml: ${field}.message_retention_period must be ${MIN_MESSAGE_RETENTION_SECS}..=${MAX_MESSAGE_RETENTION_SECS} seconds`,
    );
  }
}

/** Longest queue name. */
export const MAX_QUEUE_NAME_LEN = 63;
/** Queue-name prefixes only the host may use. */
export const RESERVED_QUEUE_PREFIXES = ["core.", "node."] as const;

/**
 * True when `name` is a valid queue name: `[a-z0-9][a-z0-9._-]*`, at most
 * {@link MAX_QUEUE_NAME_LEN} characters.
 *
 * @param name - Candidate queue name.
 * @returns Whether the grammar accepts it.
 */
export function isValidQueueName(name: string): boolean {
  return (
    name.length > 0 && name.length <= MAX_QUEUE_NAME_LEN && /^[a-z0-9][a-z0-9._-]*$/.test(name)
  );
}

function validateQueueName(field: string, name: unknown): void {
  const text = typeof name === "string" ? name : "";
  if (!isValidQueueName(text)) {
    throw new Error(
      `plugin.toml: ${field} \`${text}\` must be \`[a-z0-9][a-z0-9._-]*\` and at most ${MAX_QUEUE_NAME_LEN} chars`,
    );
  }
  if (RESERVED_QUEUE_PREFIXES.some((prefix) => text.startsWith(prefix))) {
    throw new Error(
      `plugin.toml: ${field} \`${text}\` uses a host-reserved prefix (\`core.\`, \`node.\`)`,
    );
  }
}

/**
 * Domain event types only the host emits (`book_acquired` is written by the
 * acquire pipeline once a source's acquire handler returns; `edition_linked`,
 * `edition_updated`, `holding_added` and `holding_removed` by the library
 * model; `edition_acquire_changed` when an edition's acquire state moves to
 * anything but acquired;
 * `wishlist_changed` and `listening_synced` by the library when a wishlist
 * entry or someone's listening changes; `playback_started`,
 * `playback_progress` and `playback_finished` by the built-in player).
 * Plugins consume them but cannot declare them under `[[events.producers]]`.
 */
export const HOST_EVENT_TYPES: ReadonlySet<string> = new Set([
  "book_acquired",
  "edition_acquire_changed",
  "edition_linked",
  "edition_updated",
  "holding_added",
  "holding_removed",
  "wishlist_changed",
  "listening_synced",
  "playback_started",
  "playback_progress",
  "playback_finished",
]);

/** Event type grammar: snake_case `[a-z][a-z0-9_]*`, as the host checks it. */
const EVENT_TYPE_RE = /^[a-z][a-z0-9_]*$/;

/** Largest `max_value_bytes` a `[[kv_namespaces]]` row may declare (1 MiB). */
export const MAX_KV_NAMESPACE_VALUE_BYTES = 1_048_576;
/** Largest `max_keys` a `[[kv_namespaces]]` row may declare. */
export const MAX_KV_NAMESPACE_KEYS = 1_000_000;

function validateKvNamespaces(m: Manifest): void {
  for (const [i, row] of (m.kv_namespaces ?? []).entries()) {
    const namespace = row.namespace ?? String(row.binding || "KV").toLowerCase();
    if (!isValidQueueName(namespace)) {
      throw new Error(
        `plugin.toml: kv_namespaces[${i}].namespace \`${namespace}\` must be \`[a-z0-9][a-z0-9._-]*\` and at most ${MAX_QUEUE_NAME_LEN} chars`,
      );
    }
    const maxValue = row.max_value_bytes ?? 65_536;
    if (!Number.isInteger(maxValue) || maxValue < 1 || maxValue > MAX_KV_NAMESPACE_VALUE_BYTES) {
      throw new Error(
        `plugin.toml: kv_namespaces[${i}].max_value_bytes must be 1..=${MAX_KV_NAMESPACE_VALUE_BYTES}`,
      );
    }
    const maxKeys = row.max_keys ?? 10_000;
    if (!Number.isInteger(maxKeys) || maxKeys < 1 || maxKeys > MAX_KV_NAMESPACE_KEYS) {
      throw new Error(
        `plugin.toml: kv_namespaces[${i}].max_keys must be 1..=${MAX_KV_NAMESPACE_KEYS}`,
      );
    }
  }
}

function validateQueues(m: Manifest): void {
  for (const [i, producer] of (m.queues?.producers ?? []).entries()) {
    validateQueueName(`queues.producers[${i}].queue`, producer.queue);
    if (!producer.binding || !String(producer.binding).trim()) {
      throw new Error(`plugin.toml: queues.producers[${i}].binding is required`);
    }
  }
  const seen = new Set<string>();
  for (const [i, consumer] of (m.queues?.consumers ?? []).entries()) {
    validateQueueName(`queues.consumers[${i}].queue`, consumer.queue);
    if (seen.has(consumer.queue)) {
      throw new Error(
        `plugin.toml: queues.consumers[${i}].queue \`${consumer.queue}\` is duplicated`,
      );
    }
    seen.add(consumer.queue);
    const batch = consumer.max_batch_size ?? 10;
    if (!Number.isInteger(batch) || batch < 1 || batch > 100) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_batch_size must be 1..=100`);
    }
    const timeout = consumer.max_batch_timeout ?? 5;
    if (!Number.isInteger(timeout) || timeout < 0 || timeout > 60) {
      throw new Error(
        `plugin.toml: queues.consumers[${i}].max_batch_timeout must be at most 60 seconds`,
      );
    }
    const retries = consumer.max_retries ?? 3;
    if (!Number.isInteger(retries) || retries < 0 || retries > 100) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_retries must be at most 100`);
    }
    const delay = consumer.retry_delay ?? 0;
    if (!Number.isInteger(delay) || delay < 0 || delay > 43200) {
      throw new Error(
        `plugin.toml: queues.consumers[${i}].retry_delay must be at most 43200 seconds`,
      );
    }
    if (
      consumer.max_concurrency != null &&
      (!Number.isInteger(consumer.max_concurrency) || consumer.max_concurrency < 1)
    ) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_concurrency must be at least 1`);
    }
    validateMessageRetention(`queues.consumers[${i}]`, consumer.message_retention_period);
    if (consumer.dead_letter_queue != null) {
      validateQueueName(`queues.consumers[${i}].dead_letter_queue`, consumer.dead_letter_queue);
      if (consumer.dead_letter_queue === consumer.queue) {
        throw new Error(
          `plugin.toml: queues.consumers[${i}].dead_letter_queue must differ from the consumed queue`,
        );
      }
    }
  }
}

/** Value types accepted in `[[settings]] type`. */
export const SETTING_TYPES = ["string", "bool", "integer", "enum", "secret", "table"] as const;

/** Value types a `[[settings.columns]]` entry of a `table` setting may have. */
export const COLUMN_TYPES = ["string", "bool", "integer", "enum"] as const;

/** Most columns one `table` setting may declare. */
const MAX_TABLE_COLUMNS = 16;

/** Most rows one `table` value may hold. */
const MAX_TABLE_ROWS = 64;

/** One cell of a `table` setting row. */
export type SettingCellToml = string | number | boolean;

/** One `[[settings.columns]]` entry: a record field every row of a `table` setting has. */
export type SettingColumnToml = {
  /** Field name (`^[a-z][a-z0-9_]{0,63}$`), unique within the table. */
  key: string;
  /** Column heading; defaults to the key. */
  label?: string;
  /** Value type of the cells. */
  type: (typeof COLUMN_TYPES)[number];
  /** Allowed values for `type = "enum"`. */
  values?: Array<{ id: string; label?: string }>;
  /** Value a row takes when it leaves the column out. */
  default?: SettingCellToml;
  /** When true, every row must have a value (from the row or `default`). */
  required?: boolean;
  /** Optional help text. */
  description?: string;
};

/** One `[[settings]]` row: an operator setting the host validates and renders. */
export type SettingToml = {
  /** Setting key (`^[a-z][a-z0-9_]{0,63}$`), unique within the plugin. */
  key: string;
  /** Operator-facing label; defaults to the key. */
  label?: string;
  /** Value type; `secret` values arrive in `SECRETS`, never in `CONFIG`. */
  type: (typeof SETTING_TYPES)[number];
  /** Allowed values for `type = "enum"`. */
  values?: Array<{ id: string; label?: string }>;
  /**
   * Default applied when the instance document omits the key (not for
   * secrets); an array of row tables for `type = "table"`.
   */
  default?: SettingCellToml | Array<Record<string, SettingCellToml>>;
  /** When true, a spawn without a value fails. */
  required?: boolean;
  /** Optional help text. */
  description?: string;
  /** Columns of a `type = "table"` setting, in display order. */
  columns?: SettingColumnToml[];
};

/**
 * Validates an enum entry's `values`, or that a non-enum declares none.
 *
 * @param where - Message prefix naming the entry.
 * @param type - The entry's declared type.
 * @param values - The entry's `values`.
 * @throws {Error} When an enum has no values or a duplicated or empty id, or a
 *   non-enum declares values.
 */
function validateEnumValues(
  where: string,
  type: string,
  values: Array<{ id: string; label?: string }>,
): void {
  if (type === "enum") {
    if (values.length === 0) {
      throw new Error(`plugin.toml: [[settings]] enum ${where} must declare \`values\``);
    }
    const ids = new Set<string>();
    for (const value of values) {
      const id = String(value.id ?? "");
      if (!id || ids.has(id)) {
        throw new Error(
          `plugin.toml: [[settings]] enum ${where} has an invalid or duplicated value id`,
        );
      }
      ids.add(id);
    }
  } else if (values.length > 0) {
    throw new Error(`plugin.toml: [[settings]] ${where} declares \`values\` but is not an enum`);
  }
}

/**
 * Whether `value` fits a scalar entry of `type`.
 *
 * @param type - Declared scalar type.
 * @param values - Allowed ids of an enum.
 * @param value - Candidate value.
 * @returns True when the value has the type's JSON shape (and is an allowed id).
 */
function scalarFits(
  type: string,
  values: Array<{ id: string; label?: string }>,
  value: unknown,
): boolean {
  return type === "bool"
    ? typeof value === "boolean"
    : type === "integer"
      ? typeof value === "number" && Number.isInteger(value)
      : type === "enum"
        ? typeof value === "string" && values.some((v) => v.id === value)
        : typeof value === "string";
}

/**
 * Validates the columns of a `table` setting and, when given, its default rows.
 *
 * @param key - The table setting's key.
 * @param spec - The table setting.
 * @throws {Error} When the columns are missing, too many, duplicated, not
 *   scalar, or badly declared, or the default rows do not fit them.
 */
function validateTableColumns(key: string, spec: SettingToml): void {
  const columns = spec.columns ?? [];
  if (columns.length === 0) {
    throw new Error(`plugin.toml: [[settings]] table setting \`${key}\` must declare \`columns\``);
  }
  if (columns.length > MAX_TABLE_COLUMNS) {
    throw new Error(
      `plugin.toml: [[settings]] table setting \`${key}\` declares more than ${MAX_TABLE_COLUMNS} columns`,
    );
  }
  const seen = new Set<string>();
  for (const column of columns) {
    const name = String(column.key ?? "");
    const where = `table setting \`${key}\` column \`${name}\``;
    if (!/^[a-z][a-z0-9_]{0,63}$/.test(name)) {
      throw new Error(`plugin.toml: [[settings]] ${where} must match ^[a-z][a-z0-9_]{0,63}$`);
    }
    if (seen.has(name)) {
      throw new Error(`plugin.toml: [[settings]] ${where} is duplicated`);
    }
    seen.add(name);
    if (!(COLUMN_TYPES as readonly string[]).includes(String(column.type))) {
      throw new Error(`plugin.toml: [[settings]] ${where} must be a string, bool, integer or enum`);
    }
    if ("columns" in column) {
      throw new Error(`plugin.toml: [[settings]] ${where} may not declare columns`);
    }
    const values = column.values ?? [];
    validateEnumValues(where, column.type, values);
    if (column.default !== undefined && !scalarFits(column.type, values, column.default)) {
      throw new Error(`plugin.toml: [[settings]] default for ${where} does not match its type`);
    }
  }
  if (spec.default === undefined) return;
  const rows = spec.default;
  const fits =
    Array.isArray(rows) &&
    rows.length <= MAX_TABLE_ROWS &&
    rows.every(
      (row) =>
        !!row &&
        typeof row === "object" &&
        !Array.isArray(row) &&
        Object.entries(row).every(([field, cell]) => {
          const column = columns.find((c) => c.key === field);
          return !!column && scalarFits(column.type, column.values ?? [], cell);
        }) &&
        columns.every(
          (column) => !column.required || column.default !== undefined || fieldIn(row, column.key),
        ),
    );
  if (!fits) {
    throw new Error(
      `plugin.toml: [[settings]] default for setting \`${key}\` does not match its columns`,
    );
  }
}

/**
 * Whether a default row sets `field`.
 *
 * @param row - Row table from a `table` default.
 * @param field - Column key.
 * @returns True when the row has an own entry for the field.
 */
function fieldIn(row: object, field: string): boolean {
  return Object.prototype.hasOwnProperty.call(row, field);
}

/**
 * Validates `[[settings]]` the way `bookclerk-plugin-manifest` does.
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a key, type, enum value list, or default is invalid.
 */
function validateSettings(m: Manifest): void {
  const settings = m.settings ?? [];
  if (settings.length > 32) {
    throw new Error("plugin.toml: [[settings]] at most 32 settings may be declared");
  }
  const seen = new Set<string>();
  for (const spec of settings) {
    const key = String(spec.key ?? "");
    if (!/^[a-z][a-z0-9_]{0,63}$/.test(key)) {
      throw new Error(
        `plugin.toml: [[settings]] settings key \`${key}\` must match ^[a-z][a-z0-9_]{0,63}$`,
      );
    }
    if (seen.has(key)) {
      throw new Error(`plugin.toml: [[settings]] settings key \`${key}\` is duplicated`);
    }
    seen.add(key);
    if (!(SETTING_TYPES as readonly string[]).includes(String(spec.type))) {
      throw new Error(
        `plugin.toml: [[settings]] setting \`${key}\` type must be one of ${SETTING_TYPES.join(", ")}`,
      );
    }
    const values = spec.values ?? [];
    validateEnumValues(`setting \`${key}\``, spec.type, values);
    if (spec.type === "table") {
      validateTableColumns(key, spec);
    } else if (spec.columns !== undefined) {
      throw new Error(
        `plugin.toml: [[settings]] setting \`${key}\` declares \`columns\` but is not a table`,
      );
    }
    if (spec.type === "secret") {
      if (spec.default !== undefined) {
        throw new Error(
          `plugin.toml: [[settings]] secret setting \`${key}\` may not declare a default`,
        );
      }
      if (!m.secrets) {
        throw new Error(
          `plugin.toml: [[settings]] secret setting \`${key}\` requires the \`[secrets]\` binding`,
        );
      }
      continue;
    }
    if (m.vars === undefined) {
      throw new Error(
        `plugin.toml: [[settings]] setting \`${key}\` requires the \`[vars]\` binding`,
      );
    }
    const def = spec.default;
    if (def === undefined || spec.type === "table") continue;
    if (!scalarFits(spec.type, values, def)) {
      throw new Error(
        `plugin.toml: [[settings]] default for setting \`${key}\` does not match its type`,
      );
    }
  }
}

/** One `[[metadata_files.dialects]]` row. */
export type MetadataFileDialectToml = {
  /** Dialect id (`[a-z][a-z0-9_]{0,63}`), unique within the plugin. */
  id?: string;
  /** Name shown in settings and on a book's files list. */
  name?: string;
  /** Exact file names the dialect reads, compared without ASCII case. */
  file_names?: string[];
  /** File name endings (each starting with `.`), compared without ASCII case. */
  suffixes?: string[];
  /** Fields the format carries (wire names such as `title`, `publishedAt`). */
  fields?: string[];
  /** Credit roles the format carries; required exactly when `fields` names `credits`. */
  credit_roles?: string[];
  /** Whether the dialect also writes files. */
  writes?: boolean;
};

/** Fields a `[[metadata_files.dialects]].fields` entry may name. */
const METADATA_FIELDS = [
  "title",
  "subtitle",
  "description",
  "publisher",
  "publishedAt",
  "language",
  "genres",
  "tags",
  "abridgement",
  "expressionKind",
  "identifiers",
  "credits",
  "series",
  "chapters",
  "duration",
];

/** Credit roles a `[[metadata_files.dialects]].credit_roles` entry may name. */
const CREDIT_ROLES = [
  "author",
  "narrator",
  "translator",
  "editor",
  "illustrator",
  "publisher",
  "host",
  "cast",
  "adaptor",
];

/** Dialect id grammar, as the host's `is_dialect_id`. */
const DIALECT_ID = /^[a-z][a-z0-9_]{0,63}$/;

/**
 * True when `name` is printable ASCII naming one file with no folder, as the
 * host's `is_ascii_file_name`.
 *
 * @param name - File name or suffix as written in `plugin.toml`.
 * @returns Whether the host would accept it.
 */
function isAsciiFileName(name: string): boolean {
  return (
    /^[\x20-\x7e]+$/.test(name) &&
    name.length <= 255 &&
    name !== "." &&
    name !== ".." &&
    !name.includes("/") &&
    !name.includes("\\")
  );
}

/**
 * Checks `[ingress]` against the entrypoints, with the host's rules and
 * messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When an `ingress` plugin has no table, another plugin has
 *   one, or the table has an unknown key or a `kind` other than `relay` or
 *   `direct`.
 */
function validateIngress(m: Manifest): void {
  const exported = ((m.entrypoints ?? []) as readonly string[]).includes("ingress");
  const table = m.ingress;
  if (table == null) {
    if (exported) {
      throw new Error(
        'plugin.toml: `[ingress]` is required for a plugin with the `ingress` entrypoint (set `kind` to "relay" or "direct")',
      );
    }
    return;
  }
  if (!exported) {
    throw new Error("plugin.toml: `[ingress]` needs the `ingress` entrypoint");
  }
  for (const key of Object.keys(table)) {
    if (key !== "kind") throw new Error(`plugin.toml: \`[ingress]\` has unknown key \`${key}\``);
  }
  if (table.kind !== "relay" && table.kind !== "direct") {
    throw new Error('plugin.toml: `[ingress].kind` must be "relay" or "direct"');
  }
}

/** Keys a `[provisioning]` table may carry. */
const PROVISIONING_KEYS = new Set([
  "provider",
  "grants",
  "token_url",
  "token_permissions",
  "inputs",
  "outputs",
  "deprovision",
]);

/** Grant kinds a `[provisioning]` table may list. */
const PROVISION_GRANTS = new Set(["api_token"]);

/**
 * Whether `text` holds a control character (Unicode `Cc`).
 *
 * @param text - Text to check.
 * @returns True when any character is a control character.
 */
function hasControl(text: string): boolean {
  return /\p{Cc}/u.test(text);
}

/**
 * Checks `[provisioning]` against the entrypoints and settings, with the
 * host's rules and messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a `provisioning` plugin has no table or another plugin
 *   has one, or the provider, grants, token URL, token permissions, inputs or
 *   outputs break their rules.
 */
function validateProvisioning(m: Manifest): void {
  const exported = ((m.entrypoints ?? []) as readonly string[]).includes("provisioning");
  const table = m.provisioning;
  if (table == null) {
    if (exported) {
      throw new Error(
        "plugin.toml: `[provisioning]` is required for a plugin with the `provisioning` entrypoint",
      );
    }
    return;
  }
  if (!exported) {
    throw new Error("plugin.toml: `[provisioning]` needs the `provisioning` entrypoint");
  }
  for (const key of Object.keys(table)) {
    if (!PROVISIONING_KEYS.has(key)) {
      throw new Error(`plugin.toml: \`[provisioning]\` has unknown key \`${key}\``);
    }
  }
  if (typeof table.provider !== "string" || !/^[a-z][a-z0-9-]{0,31}$/.test(table.provider)) {
    throw new Error("plugin.toml: `[provisioning].provider` must match ^[a-z][a-z0-9-]{0,31}$");
  }
  const grants = table.grants;
  if (
    !Array.isArray(grants) ||
    grants.length === 0 ||
    grants.some((grant) => !PROVISION_GRANTS.has(grant)) ||
    new Set(grants).size !== grants.length
  ) {
    throw new Error(
      "plugin.toml: `[provisioning].grants` must list each grant kind once and not be empty",
    );
  }
  const url = table.token_url;
  if (
    url != null &&
    (typeof url !== "string" ||
      !url.startsWith("https://") ||
      new TextEncoder().encode(url).length > 2048 ||
      /\s/u.test(url) ||
      hasControl(url))
  ) {
    throw new Error("plugin.toml: `[provisioning].token_url` must be an https:// URL");
  }
  const permissions = table.token_permissions ?? [];
  if (
    !Array.isArray(permissions) ||
    permissions.length > 16 ||
    !permissions.every(
      (line) =>
        typeof line === "string" &&
        line.trim() !== "" &&
        [...line].length <= 120 &&
        !hasControl(line),
    )
  ) {
    throw new Error(
      "plugin.toml: `[provisioning].token_permissions` takes at most 16 non-empty lines of at most 120 characters",
    );
  }
  const kinds = new Map<string, string>();
  for (const setting of m.settings ?? []) kinds.set(String(setting.key), String(setting.type));
  const seenInputs = new Set<string>();
  for (const key of table.inputs ?? []) {
    const kind = kinds.get(key);
    if (!["string", "bool", "integer", "enum"].includes(kind ?? "") || seenInputs.has(key)) {
      throw new Error(
        `plugin.toml: \`[provisioning].inputs\` entry \`${key}\` must be a declared setting that is not a secret or a table, listed once`,
      );
    }
    seenInputs.add(key);
  }
  const outputs = table.outputs ?? [];
  if (outputs.length === 0) {
    throw new Error("plugin.toml: `[provisioning].outputs` must not be empty");
  }
  const seenOutputs = new Set<string>();
  for (const key of outputs) {
    const kind = kinds.get(key);
    if (
      !["string", "bool", "integer", "enum", "secret"].includes(kind ?? "") ||
      seenOutputs.has(key)
    ) {
      throw new Error(
        `plugin.toml: \`[provisioning].outputs\` entry \`${key}\` must be a declared setting that is not a table, listed once`,
      );
    }
    seenOutputs.add(key);
  }
  if (table.deprovision != null && typeof table.deprovision !== "boolean") {
    throw new Error("plugin.toml: `[provisioning].deprovision` must be a boolean");
  }
}

/**
 * Checks `[metadata_files]` against the entrypoints, with the host's rules and
 * messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a `metadataFiles` plugin has no dialects or another
 *   plugin has the table, or a dialect has a bad or repeated id, no name, no
 *   file names or suffixes, a malformed or repeated name or suffix, no or
 *   unknown fields, a repeated field or credit role, or credit roles that do
 *   not match `credits`.
 */
/** Keys a `[[metadata_files.dialects]]` row may carry (the host's fields). */
const DIALECT_KEYS: ReadonlySet<string> = new Set([
  "id",
  "name",
  "file_names",
  "suffixes",
  "fields",
  "credit_roles",
  "writes",
]);

function validateMetadataFiles(m: Manifest): void {
  const exported = ((m.entrypoints ?? []) as readonly string[]).includes("metadataFiles");
  const table = m.metadata_files;
  if (table == null) {
    if (exported) {
      throw new Error(
        "plugin.toml: `[metadata_files]` is required for a plugin with the `metadataFiles` entrypoint (declare at least one dialect)",
      );
    }
    return;
  }
  if (!exported) {
    throw new Error("plugin.toml: `[metadata_files]` needs the `metadataFiles` entrypoint");
  }
  // The host parses these tables with `deny_unknown_fields`.
  for (const key of Object.keys(table)) {
    if (key !== "dialects") {
      throw new Error(`plugin.toml: \`[metadata_files]\` has unknown key \`${key}\``);
    }
  }
  const dialects = table.dialects ?? [];
  if (dialects.length === 0) {
    throw new Error("plugin.toml: `[metadata_files].dialects` is empty");
  }
  const ids = new Set<string>();
  const claimed = new Set<string>();
  dialects.forEach((dialect, i) => {
    const row = `plugin.toml: metadata_files.dialects[${i}]`;
    for (const key of Object.keys(dialect)) {
      if (!DIALECT_KEYS.has(key)) {
        throw new Error(`${row} has unknown key \`${key}\``);
      }
    }
    if (dialect.writes !== undefined && typeof dialect.writes !== "boolean") {
      throw new Error(`${row}.writes must be a boolean`);
    }
    const id = String(dialect.id ?? "");
    if (!DIALECT_ID.test(id)) {
      throw new Error(`${row}.id \`${id}\` is not an identifier ([a-z][a-z0-9_]{0,63})`);
    }
    if (ids.has(id)) {
      throw new Error(`plugin.toml: metadata_files dialect id \`${id}\` is declared twice`);
    }
    ids.add(id);
    if (!String(dialect.name ?? "").trim()) {
      throw new Error(`${row}.name is required`);
    }
    const names = dialect.file_names ?? [];
    const suffixes = dialect.suffixes ?? [];
    if (names.length === 0 && suffixes.length === 0) {
      throw new Error(`${row} declares no \`file_names\` or \`suffixes\``);
    }
    for (const name of names) {
      if (!isAsciiFileName(name)) {
        throw new Error(`${row}.file_names entry \`${name}\` is not a plain ASCII file name`);
      }
    }
    for (const suffix of suffixes) {
      if (suffix.length < 2 || !suffix.startsWith(".") || !isAsciiFileName(suffix)) {
        throw new Error(
          `${row}.suffixes entry \`${suffix}\` must start with \`.\` and be a plain ASCII file name`,
        );
      }
    }
    for (const name of [...names, ...suffixes]) {
      const key = name.toLowerCase();
      if (claimed.has(key)) {
        throw new Error(`plugin.toml: metadata_files \`${name}\` is claimed twice`);
      }
      claimed.add(key);
    }
    const fields = dialect.fields ?? [];
    for (const field of fields) {
      if (!METADATA_FIELDS.includes(field)) {
        throw new Error(`${row}.fields names unknown field \`${field}\``);
      }
    }
    const roles = dialect.credit_roles ?? [];
    for (const role of roles) {
      if (!CREDIT_ROLES.includes(role)) {
        throw new Error(`${row}.credit_roles names unknown role \`${role}\``);
      }
    }
    if (fields.length === 0) {
      throw new Error(`${row}.fields is empty`);
    }
    const seenFields = new Set<string>();
    for (const field of fields) {
      if (seenFields.has(field)) {
        throw new Error(`${row}.fields names \`${field}\` twice`);
      }
      seenFields.add(field);
    }
    const seenRoles = new Set<string>();
    for (const role of roles) {
      if (seenRoles.has(role)) {
        throw new Error(`${row}.credit_roles names \`${role}\` twice`);
      }
      seenRoles.add(role);
    }
    const credits = fields.includes("credits");
    if (credits && roles.length === 0) {
      throw new Error(`${row}.credit_roles is empty but \`fields\` names \`credits\``);
    }
    if (!credits && roles.length > 0) {
      throw new Error(`${row}.credit_roles needs \`credits\` in \`fields\``);
    }
  });
}

/** `[notifier]` as written in `plugin.toml`. */
export type NotifierToml = {
  /** Destination kinds: `channel`, `personal`; never empty. */
  destinations?: string[];
  /** Whether an instance may carry account mail (needs an `email` field). */
  account_mail?: boolean;
  /** Body formats rendered: `text` (required), `markdown`, `html`. */
  formats?: string[];
  /** Fields of a personal destination's address. */
  address_fields?: NotifierAddressFieldToml[];
};

/** One `[[notifier.address_fields]]` row. */
export type NotifierAddressFieldToml = {
  /** Key in the address document (`[a-z][a-z0-9_]{0,63}`). */
  key?: string;
  /** Label shown on the patron's form. */
  label?: string;
  /** Whether a destination must carry it. */
  required?: boolean;
  /** Whether the value is a secret, shown masked and never logged. */
  secret?: boolean;
};

/** One `[[notifications.types]]` row. */
export type NotificationTypeToml = {
  /** Type id (`[a-z][a-z0-9_]{0,63}`), routed as `<plugin id>:<id>`. */
  id?: string;
  /** `operational` or `patron`; `account` is reserved to Bookclerk. */
  audience?: string;
  /** Default severity: `info`, `warning` or `critical`. */
  severity?: string;
  /** Title shown in the routing matrix and on each notification. */
  title?: string;
};

/** Most `[[notifier.address_fields]]` rows (the host's address key cap). */
const MAX_NOTIFIER_ADDRESS_FIELDS = 16;

/** Most `[[notifications.types]]` rows per plugin. */
const MAX_NOTIFICATION_TYPES = 64;

/** Longest notification type title, in UTF-8 bytes. */
const MAX_NOTIFICATION_TITLE_BYTES = 256;

/** Keys `[notifier]` may carry (the host's fields). */
const NOTIFIER_KEYS: ReadonlySet<string> = new Set([
  "destinations",
  "account_mail",
  "formats",
  "address_fields",
]);

/** Keys a `[[notifier.address_fields]]` row may carry. */
const ADDRESS_FIELD_KEYS: ReadonlySet<string> = new Set(["key", "label", "required", "secret"]);

/** Keys a `[[notifications.types]]` row may carry. */
const NOTIFICATION_TYPE_KEYS: ReadonlySet<string> = new Set([
  "id",
  "audience",
  "severity",
  "title",
]);

/**
 * Checks `[notifier]` against the entrypoints, with the host's rules and
 * messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a `notifier` plugin has no table or another plugin has
 *   one, or the table has no, unknown or repeated destinations, formats
 *   without `text`, address fields that do not match `personal`, a bad,
 *   repeated or unlabelled field, no required field, or `account_mail`
 *   without a required, non-secret `email` field.
 */
function validateNotifier(m: Manifest): void {
  const exported = ((m.entrypoints ?? []) as readonly string[]).includes("notifier");
  const table = m.notifier;
  if (table == null) {
    if (exported) {
      throw new Error(
        "plugin.toml: `[notifier]` is required for a plugin with the `notifier` entrypoint",
      );
    }
    return;
  }
  if (!exported) {
    throw new Error("plugin.toml: `[notifier]` needs the `notifier` entrypoint");
  }
  for (const key of Object.keys(table)) {
    if (!NOTIFIER_KEYS.has(key)) {
      throw new Error(`plugin.toml: \`[notifier]\` has unknown key \`${key}\``);
    }
  }
  if (table.account_mail !== undefined && typeof table.account_mail !== "boolean") {
    throw new Error("plugin.toml: `[notifier].account_mail` must be a boolean");
  }
  const destinations = table.destinations ?? [];
  for (const kind of destinations) {
    if (kind !== "channel" && kind !== "personal") {
      throw new Error(`plugin.toml: \`[notifier].destinations\` names unknown kind \`${kind}\``);
    }
  }
  if (destinations.length === 0) {
    throw new Error("plugin.toml: `[notifier].destinations` is empty");
  }
  if (new Set(destinations).size !== destinations.length) {
    throw new Error("plugin.toml: `[notifier].destinations` names a kind twice");
  }
  const formats = table.formats ?? ["text"];
  for (const format of formats) {
    if (format !== "text" && format !== "markdown" && format !== "html") {
      throw new Error(`plugin.toml: \`[notifier].formats\` names unknown format \`${format}\``);
    }
  }
  if (new Set(formats).size !== formats.length) {
    throw new Error("plugin.toml: `[notifier].formats` names a format twice");
  }
  if (!formats.includes("text")) {
    throw new Error("plugin.toml: `[notifier].formats` must include `text`");
  }
  const fields = table.address_fields ?? [];
  const personal = destinations.includes("personal");
  if (personal && fields.length === 0) {
    throw new Error(
      "plugin.toml: `[notifier]` sends to `personal` destinations but declares no `[[notifier.address_fields]]`",
    );
  }
  if (!personal && fields.length > 0) {
    throw new Error(
      "plugin.toml: `[[notifier.address_fields]]` needs `personal` in `[notifier].destinations`",
    );
  }
  if (fields.length > MAX_NOTIFIER_ADDRESS_FIELDS) {
    throw new Error(
      `plugin.toml: \`[[notifier.address_fields]]\` declares more than ${MAX_NOTIFIER_ADDRESS_FIELDS} fields`,
    );
  }
  const keys = new Set<string>();
  fields.forEach((field, i) => {
    const row = `plugin.toml: notifier.address_fields[${i}]`;
    for (const key of Object.keys(field)) {
      if (!ADDRESS_FIELD_KEYS.has(key)) {
        throw new Error(`${row} has unknown key \`${key}\``);
      }
    }
    for (const flag of ["required", "secret"] as const) {
      if (field[flag] !== undefined && typeof field[flag] !== "boolean") {
        throw new Error(`${row}.${flag} must be a boolean`);
      }
    }
    const key = String(field.key ?? "");
    if (!DIALECT_ID.test(key)) {
      throw new Error(`${row}.key \`${key}\` is not an identifier ([a-z][a-z0-9_]{0,63})`);
    }
    if (keys.has(key)) {
      throw new Error(`plugin.toml: notifier address field \`${key}\` is declared twice`);
    }
    keys.add(key);
    if (!String(field.label ?? "").trim()) {
      throw new Error(`${row}.label is required`);
    }
  });
  if (personal && !fields.some((field) => field.required === true)) {
    throw new Error("plugin.toml: `[[notifier.address_fields]]` needs at least one required field");
  }
  if (
    table.account_mail === true &&
    !fields.some((f) => f.key === "email" && f.required === true && f.secret !== true)
  ) {
    throw new Error(
      "plugin.toml: `[notifier].account_mail` needs a required, non-secret `email` address field",
    );
  }
}

/**
 * Checks `[[notifications.types]]`, with the host's rules and messages
 * (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} For too many rows, unknown keys, a bad or repeated id, an
 *   unknown or `account` audience, an unknown severity, or a blank, multi-line
 *   or over-long title.
 */
function validateNotificationTypes(m: Manifest): void {
  const table = m.notifications;
  if (table == null) return;
  for (const key of Object.keys(table)) {
    if (key !== "types") {
      throw new Error(`plugin.toml: \`[notifications]\` has unknown key \`${key}\``);
    }
  }
  const types = table.types ?? [];
  if (types.length > MAX_NOTIFICATION_TYPES) {
    throw new Error(
      `plugin.toml: \`[[notifications.types]]\` declares more than ${MAX_NOTIFICATION_TYPES} types`,
    );
  }
  const ids = new Set<string>();
  types.forEach((row_type, i) => {
    const row = `plugin.toml: notifications.types[${i}]`;
    for (const key of Object.keys(row_type)) {
      if (!NOTIFICATION_TYPE_KEYS.has(key)) {
        throw new Error(`${row} has unknown key \`${key}\``);
      }
    }
    const id = String(row_type.id ?? "");
    if (!DIALECT_ID.test(id)) {
      throw new Error(`${row}.id \`${id}\` is not an identifier ([a-z][a-z0-9_]{0,63})`);
    }
    if (ids.has(id)) {
      throw new Error(`plugin.toml: notification type \`${id}\` is declared twice`);
    }
    ids.add(id);
    const audience = String(row_type.audience ?? "");
    if (audience === "account") {
      throw new Error(`${row}.audience \`account\` is reserved to Bookclerk`);
    }
    if (audience !== "operational" && audience !== "patron") {
      throw new Error(`${row}.audience must be \`operational\` or \`patron\``);
    }
    const severity = row_type.severity ?? "info";
    if (severity !== "info" && severity !== "warning" && severity !== "critical") {
      throw new Error(`${row}.severity must be \`info\`, \`warning\` or \`critical\``);
    }
    const title = typeof row_type.title === "string" ? row_type.title : "";
    // eslint-disable-next-line no-control-regex
    const control = /[\u0000-\u001f\u007f-\u009f]/.test(title);
    const bytes = new TextEncoder().encode(title).length;
    if (!title.trim() || bytes > MAX_NOTIFICATION_TITLE_BYTES || control) {
      throw new Error(
        `${row}.title must be one line of 1 to ${MAX_NOTIFICATION_TITLE_BYTES} bytes`,
      );
    }
  });
}

/** Model tasks `[ai].serves` and `[ai].calls` may name. */
export const AI_TASK_NAMES = ["textEmbeddings"] as const;

/** Plugin id that names task aliases (`@library/text-embeddings`). */
const AI_ALIAS_NAMESPACE = "library";

/**
 * Checks `[ai]` against the entrypoints, with the host's rules and messages
 * (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a plugin with the `ai` entrypoint serves no task or
 *   another plugin serves one, for unknown keys or tasks, a task named twice,
 *   or a model plugin whose id is `library`.
 */
function validateAi(m: Manifest): void {
  const exported = ((m.entrypoints ?? []) as readonly string[]).includes("ai");
  const table = m.ai ?? {};
  for (const key of Object.keys(table)) {
    if (key !== "serves" && key !== "calls") {
      throw new Error(`plugin.toml: \`[ai]\` has unknown key \`${key}\``);
    }
  }
  const serves = table.serves ?? [];
  const calls = table.calls ?? [];
  if (exported && serves.length === 0) {
    throw new Error("plugin.toml: `[ai].serves` is required for a plugin with the `ai` entrypoint");
  }
  if (!exported && serves.length > 0) {
    throw new Error("plugin.toml: `[ai].serves` needs the `ai` entrypoint");
  }
  if (exported && m.id === AI_ALIAS_NAMESPACE) {
    throw new Error(
      `plugin.toml: a plugin with the \`ai\` entrypoint cannot have the id \`${AI_ALIAS_NAMESPACE}\`, which names task aliases`,
    );
  }
  for (const [list, tasks] of [
    ["serves", serves],
    ["calls", calls],
  ] as const) {
    const seen = new Set<string>();
    for (const task of tasks) {
      if (!(AI_TASK_NAMES as readonly string[]).includes(task)) {
        throw new Error(`plugin.toml: \`[ai].${list}\` names unknown task \`${task}\``);
      }
      if (seen.has(task)) {
        throw new Error(`plugin.toml: \`[ai].${list}\` names the task \`${task}\` twice`);
      }
      seen.add(task);
    }
  }
}

/** Entrypoint wire names accepted in `entrypoints`. */
export const ENTRYPOINT_NAMES = [
  "storefront",
  "storage",
  "databaseAdapter",
  "remoteLibrary",
  "cli",
  "oidc",
  "metadata",
  "covers",
  "metadataFiles",
  "notifier",
  "ingress",
  "ai",
  "provisioning",
] as const;

const LOGO_EXTENSIONS = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".ico"] as const;

/**
 * Validates a plugin id against the strict grammar.
 *
 * Mirrors Rust `validate_plugin_id`: `[a-z0-9_]{2,32}` with no leading/trailing
 * `_` and no `__`. Ids are globally unique across kinds. Leading/trailing
 * whitespace is rejected (non-lossy), not trimmed.
 *
 * @param id - Candidate plugin id from `plugin.toml`.
 * @throws {Error} When the id violates the grammar.
 *
 * @example
 * ```ts
 * validatePluginId("echo_source");
 * ```
 */
export function validatePluginId(id: string): void {
  if (id !== id.trim()) {
    throw new Error(`plugin id \`${id}\` must not have leading or trailing whitespace`);
  }
  if (id.length < 2 || id.length > 32) {
    throw new Error(`plugin id \`${id}\` must be 2–32 characters`);
  }
  if (![...id].every((c) => /[a-z0-9_]/.test(c))) {
    throw new Error(`plugin id \`${id}\` must be lowercase ascii letters, digits, or \`_\``);
  }
  if (id.startsWith("_") || id.endsWith("_") || id.includes("__")) {
    throw new Error(`plugin id \`${id}\` must not start/end with \`_\` or contain \`__\``);
  }
}

/**
 * Classifies and validates a `plugin.toml` `logo` value.
 *
 * Mirrors Rust `validate_logo`: either an `http(s)` URL or a relative embedded
 * image path under the plugin root.
 *
 * @param raw - Raw logo string from the manifest.
 * @returns Discriminated logo kind with the validated value.
 * @throws {Error} When the logo is empty, unsafe, or uses a disallowed scheme.
 */
export function validateLogo(
  raw: string,
): { kind: "remote"; value: string } | { kind: "embedded"; value: string } {
  const trimmed = raw.trim();
  if (!trimmed) {
    throw new Error("plugin.toml: `logo` must not be empty (omit the key instead)");
  }
  if (trimmed.includes("\0")) {
    throw new Error("plugin.toml: `logo` must not contain NUL");
  }
  // Absolute URLs (any scheme) via WHATWG URL — only http/https allowed.
  // Relative image paths throw from the constructor and use path validation.
  // Do not catch errors from validateParsedUrl (scheme/userinfo/host checks).
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    return validateEmbeddedPath(trimmed);
  }
  return validateParsedUrl(parsed, trimmed);
}

function validateParsedUrl(parsed: URL, original: string): { kind: "remote"; value: string } {
  const scheme = parsed.protocol.replace(/:$/, "").toLowerCase();
  if (scheme !== "http" && scheme !== "https") {
    throw new Error(
      `plugin.toml: \`logo\` URL must use http:// or https:// (got scheme \`${scheme}\`)`,
    );
  }
  // Align with Rust `url::Url`: require an explicit `://` authority. WHATWG
  // normalizes forms like `http:example.com` into a host URL; the Rust parser
  // does not treat those as remote logos with a host.
  if (!/^https?:\/\//i.test(original.trim())) {
    throw new Error(
      `plugin.toml: \`logo\` URL must use http:// or https:// (got scheme \`${scheme}\`)`,
    );
  }
  if (parsed.username || parsed.password) {
    throw new Error("plugin.toml: `logo` URL must not include userinfo (user:pass@host)");
  }
  const host = parsed.hostname.trim();
  if (!host || host === "." || host === "..") {
    throw new Error("plugin.toml: `logo` URL is missing a host");
  }
  return { kind: "remote", value: original };
}

function validateEmbeddedPath(trimmed: string): { kind: "embedded"; value: string } {
  const path = trimmed.replace(/\\/g, "/");
  if (path.startsWith("/") || path.startsWith("~")) {
    throw new Error("plugin.toml: embedded `logo` must be a relative path under the plugin root");
  }
  if (path.length >= 2 && path.charCodeAt(1) === 58 /* : */) {
    throw new Error("plugin.toml: embedded `logo` must be a relative path (no drive letter)");
  }
  if (path.startsWith("//")) {
    throw new Error("plugin.toml: embedded `logo` must be a relative path (no UNC)");
  }
  const segments: string[] = [];
  for (const seg of path.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") {
      throw new Error("plugin.toml: embedded `logo` must not contain `..` segments");
    }
    segments.push(seg);
  }
  if (segments.length === 0) {
    throw new Error("plugin.toml: embedded `logo` path is empty after normalization");
  }
  const normalized = segments.join("/");
  const lowerPath = normalized.toLowerCase();
  if (!LOGO_EXTENSIONS.some((ext) => lowerPath.endsWith(ext))) {
    throw new Error(
      `plugin.toml: embedded \`logo\` must end with one of ${LOGO_EXTENSIONS.join(", ")}`,
    );
  }
  return { kind: "embedded", value: normalized };
}

/**
 * Validates a parsed {@link Manifest} against semantic manifest rules.
 *
 * Checks id grammar, `api_version`, entrypoints / triggers, runtime-specific
 * required fields, and workerd network domain requirements.
 *
 * @param m - Manifest object (typically from `smol-toml`).
 * @throws {Error} When any semantic rule fails (message prefixed with `plugin.toml:`).
 */
export function validateManifest(m: Manifest): void {
  if (!m.id || !String(m.id).trim()) {
    throw new Error("plugin.toml: `id` is required");
  }
  try {
    // Validate the raw id (non-lossy): do not trim before grammar checks.
    validatePluginId(String(m.id));
  } catch (err) {
    throw new Error(`plugin.toml: ${(err as Error).message}`);
  }
  if (m.api_version !== 3) {
    throw new Error("plugin.toml: `api_version` must be 3");
  }
  if (m.logo != null) {
    validateLogo(String(m.logo));
  }
  validateMedia(m);
  validateAcquire(m);
  validateMetadataFiles(m);
  validateNotifier(m);
  validateNotificationTypes(m);
  validateIngress(m);
  validateProvisioning(m);
  validateAi(m);
  validateSurface(m);
  validateSettings(m);
  validateOwnUserAgent(m);
  if (m.sockets && m.capabilities?.network?.mode !== "outbound") {
    throw new Error(
      'plugin.toml: [sockets] needs capabilities.network.mode = "outbound" (every socket URL must pass the network grant)',
    );
  }
  const runtime = m.runtime ?? "native";
  if (runtime === "native") {
    if (!m.command || !String(m.command).trim()) {
      throw new Error('plugin.toml: `command` is required when runtime = "native"');
    }
    if (m.capabilities?.network?.domains && m.capabilities.network.domains.length > 0) {
      throw new Error(
        'plugin.toml: capabilities.network.domains is only valid for runtime = "workerd" (native guests use capabilities.network.tcp for mediated connect(); omit domains)',
      );
    }
  } else if (runtime === "workerd") {
    if (!m.workerd) {
      throw new Error(
        'plugin.toml: `[workerd]` / runtime.workerd is required when runtime = "workerd"',
      );
    }
    if (typeof m.workerd.compatibility_date !== "string" || !m.workerd.compatibility_date.trim()) {
      throw new Error("plugin.toml: workerd.compatibility_date is required");
    }
    if (!m.workerd.main_module?.trim()) {
      throw new Error("plugin.toml: workerd.main_module is required");
    }
    validateAuthorCompatibilityDate(m.workerd.compatibility_date);
    validateModuleDeclarations(m.workerd.main_module, m.modules ?? []);
    validateAuthorCompatibilityFlags(m.workerd.compatibility_flags ?? [], declaresPython(m));
    if (
      m.capabilities?.network?.mode === "outbound" &&
      (!m.capabilities.network.domains || m.capabilities.network.domains.length === 0)
    ) {
      throw new Error(
        'plugin.toml: capabilities.network.domains is required when runtime = "workerd" and mode = "outbound"',
      );
    }
  } else {
    throw new Error(`plugin.toml: unknown runtime ${runtime}`);
  }
}

/** Work kinds a `[media].works` entry may name. */
const MEDIA_WORK_KINDS = ["unspecified", "book", "podcast"];

/** Carriers a `[media].lists` / `[media].fetches` entry may name. */
const MEDIA_CARRIERS = ["unspecified", "audio", "ebook", "print"];

/**
 * Checks `[media]` against the entrypoints, with the host's rules and
 * messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a storefront or metadata provider has no table or an
 *   empty `works` / `lists`, another plugin has one, a list names an unknown
 *   kind or `unspecified`, or `fetches` is declared by a metadata provider or
 *   names a carrier `lists` does not.
 */
function validateMedia(m: Manifest): void {
  const entrypoints = (m.entrypoints ?? []) as readonly string[];
  const storefront = entrypoints.includes("storefront");
  const metadata = entrypoints.includes("metadata");
  const media = m.media;
  if (media == null) {
    if (storefront || metadata) {
      throw new Error(
        "plugin.toml: `[media]` is required for a plugin with the `storefront` or `metadata` entrypoint (declare `works` and `lists`)",
      );
    }
    return;
  }
  if (!storefront && !metadata) {
    throw new Error("plugin.toml: `[media]` needs the `storefront` or `metadata` entrypoint");
  }
  const works = media.works ?? [];
  const lists = media.lists ?? [];
  const fetches = media.fetches ?? [];
  for (const [field, values, known] of [
    ["works", works, MEDIA_WORK_KINDS],
    ["lists", lists, MEDIA_CARRIERS],
    ["fetches", fetches, MEDIA_CARRIERS],
  ] as const) {
    for (const value of values) {
      if (!known.includes(value)) {
        throw new Error(`plugin.toml: \`[media].${field}\` names unknown kind \`${value}\``);
      }
    }
  }
  if (works.length === 0) {
    throw new Error("plugin.toml: `[media].works` is empty");
  }
  if (lists.length === 0) {
    throw new Error("plugin.toml: `[media].lists` is empty");
  }
  for (const [field, values] of [
    ["works", works],
    ["lists", lists],
    ["fetches", fetches],
  ] as const) {
    if (values.includes("unspecified")) {
      throw new Error(`plugin.toml: \`[media].${field}\` names \`unspecified\``);
    }
  }
  if (!storefront && fetches.length > 0) {
    throw new Error("plugin.toml: `[media].fetches` needs the `storefront` entrypoint");
  }
  const unlisted = fetches.find((kind) => !lists.includes(kind));
  if (unlisted !== undefined) {
    throw new Error(
      `plugin.toml: \`[media].fetches\` names \`${unlisted}\`, which \`lists\` does not`,
    );
  }
}

/** Each `[acquire]` field with its allowed range, in check order. */
const ACQUIRE_RANGES = [
  ["downloads_per_day", 1, 10_000],
  ["min_gap_seconds", 0, 86_400],
  ["cooldown_seconds", 1, 604_800],
] as const;

/**
 * Checks `[acquire]` against the entrypoints and its ranges, with the host's
 * rules and messages (`manifest-cases.json`).
 *
 * @param m - Manifest under validation.
 * @throws {Error} When a plugin without the `storefront` entrypoint declares
 *   the table, the table names a field the host does not know, or a value is
 *   not a whole number in its range.
 */
function validateAcquire(m: Manifest): void {
  const acquire = m.acquire;
  if (acquire == null) {
    return;
  }
  const entrypoints = (m.entrypoints ?? []) as readonly string[];
  if (!entrypoints.includes("storefront")) {
    throw new Error("plugin.toml: `[acquire]` needs the `storefront` entrypoint");
  }
  // A TOML table parses to a plain object; an array, a date (an object with
  // no keys of its own) or any other value is not one.
  const table: unknown = acquire;
  const proto: unknown =
    typeof table === "object" && table !== null ? Object.getPrototypeOf(table) : undefined;
  if (Array.isArray(table) || (proto !== Object.prototype && proto !== null)) {
    throw new Error("plugin.toml: `[acquire]` must be a table");
  }
  const known: readonly string[] = ACQUIRE_RANGES.map(([field]) => field);
  const unknown = Object.keys(acquire).filter((key) => !known.includes(key));
  if (unknown.length > 0) {
    throw new Error(
      `plugin.toml: \`[acquire]\`: unknown field \`${unknown[0]}\` (expected ${known.join(", ")})`,
    );
  }
  for (const [field, min, max] of ACQUIRE_RANGES) {
    const raw: unknown = acquire[field];
    if (raw === undefined) {
      continue;
    }
    const value = typeof raw === "bigint" ? Number(raw) : raw;
    if (typeof value !== "number" || !Number.isInteger(value) || value < min || value > max) {
      throw new Error(`plugin.toml: \`[acquire].${field}\` must be between ${min} and ${max}`);
    }
  }
}

/** Most hosts one `[capabilities.network.own_user_agent]` table may list. */
const MAX_OWN_USER_AGENT_HOSTS = 32;

/** Longest `own_user_agent.reason`, in characters. */
const MAX_OWN_USER_AGENT_REASON_CHARS = 300;

/**
 * Code points a URL host may not contain (WHATWG forbidden domain code
 * points), which the host's IDNA step rejects too.
 */
// eslint-disable-next-line no-control-regex
const FORBIDDEN_HOST_CHARS = /[\u0000-\u0020#%,/:<>?@[\\\]^|\u007f]/;

/**
 * True when `raw` is an exact hostname or a `*.` wildcard, as the host's
 * `normalize_tcp_host` accepts it: trimmed, trailing dots dropped, never `*`,
 * no port, scheme, path or percent-encoding, and valid IDNA.
 *
 * @param raw - Host as written in `plugin.toml`.
 * @returns Whether the host would accept it.
 */
function isOwnUserAgentHost(raw: string): boolean {
  const trimmed = raw.trim();
  // A scan, not `/\.+$/`, which backtracks on long runs of dots.
  let end = trimmed.length;
  while (end > 0 && trimmed[end - 1] === ".") end--;
  const pattern = trimmed.slice(0, end);
  const host = pattern.startsWith("*.") ? pattern.slice(2) : pattern;
  if (!host || host === "*" || FORBIDDEN_HOST_CHARS.test(host)) return false;
  // The host checks IDNA only. A URL also parses a last label of digits as
  // IPv4, so a known label is appended to leave that check out.
  // IDNA maps some Unicode forms (a fullwidth comma, say) to these ASCII
  // characters, so the normalized name is checked again.
  try {
    const ascii = new URL(`http://${host}.example/`).hostname;
    if (ascii.length === 0 || FORBIDDEN_HOST_CHARS.test(ascii)) return false;
    // Some URL parsers pass an `xn--` label through unchecked; the host's
    // IDNA step requires each to decode to the label it encodes.
    return ascii.split(".").every(isValidALabel);
  } catch {
    return false;
  }
}

/**
 * Validates `[capabilities.network.own_user_agent]` the way the host does:
 * outbound mode, 1 to 32 hosts that are not `*`, and a one-line reason.
 *
 * @param m - Manifest under validation.
 * @throws {Error} When the table is present and malformed.
 */
function validateOwnUserAgent(m: Manifest): void {
  const own = m.capabilities?.network?.own_user_agent;
  if (own === undefined) return;
  const field = "plugin.toml: capabilities.network.own_user_agent";
  const unknown = Object.keys(own).filter((key) => key !== "hosts" && key !== "reason");
  if (unknown.length > 0) {
    throw new Error(`${field}: unknown field \`${unknown[0]}\` (expected hosts, reason)`);
  }
  if (m.capabilities.network.mode !== "outbound") {
    throw new Error(`${field} needs mode = "outbound"`);
  }
  const hosts = Array.isArray(own.hosts) ? own.hosts : [];
  if (hosts.length === 0 || hosts.length > MAX_OWN_USER_AGENT_HOSTS) {
    throw new Error(`${field}.hosts must list between 1 and ${MAX_OWN_USER_AGENT_HOSTS} hosts`);
  }
  for (const host of hosts) {
    if (typeof host !== "string" || !isOwnUserAgentHost(host)) {
      throw new Error(
        `${field}.hosts entry \`${String(host)}\` is not a valid hostname or \`*.\` wildcard (\`*\` is not allowed)`,
      );
    }
  }
  const reason = typeof own.reason === "string" ? own.reason.trim() : "";
  // eslint-disable-next-line no-control-regex
  const hasControl = /[\u0000-\u001f\u007f-\u009f]/.test(reason);
  if (!reason || [...reason].length > MAX_OWN_USER_AGENT_REASON_CHARS || hasControl) {
    throw new Error(
      `${field}.reason must be one line of 1 to ${MAX_OWN_USER_AGENT_REASON_CHARS} characters`,
    );
  }
}

/**
 * Validates the exported surface: entrypoint names, duplicates, and the
 * "declare at least one entrypoint or trigger" rule.
 *
 * @param m - Manifest under validation.
 * @throws {Error} When the surface is empty, unknown, or inconsistent.
 */
function validateSurface(m: Manifest): void {
  const entrypoints = m.entrypoints ?? [];
  const seen = new Set<string>();
  for (const entrypoint of entrypoints) {
    if (!(ENTRYPOINT_NAMES as readonly string[]).includes(entrypoint)) {
      throw new Error(
        `plugin.toml: unknown entrypoint \`${entrypoint}\` (expected one of ${ENTRYPOINT_NAMES.join(", ")})`,
      );
    }
    if (seen.has(entrypoint)) {
      throw new Error(`plugin.toml: entrypoints entry \`${entrypoint}\` is duplicated`);
    }
    seen.add(entrypoint);
  }
  const consumers = m.events?.consumers ?? [];
  const queueConsumers = m.queues?.consumers ?? [];
  const jobs = m.triggers?.jobs ?? [];
  const crons = m.triggers?.crons ?? [];
  if (
    entrypoints.length === 0 &&
    consumers.length === 0 &&
    queueConsumers.length === 0 &&
    jobs.length === 0 &&
    crons.length === 0
  ) {
    throw new Error(
      "plugin.toml: declare at least one of `entrypoints`, `[[events.consumers]]`, `[[queues.consumers]]`, `[triggers].jobs`, or `[triggers].crons`",
    );
  }
  validateCrons(crons);
  if (Array.isArray(crons) && crons.length > 0) {
    // Crons fire only on integration deployments (host `validate`).
    for (const registry of ["storefront", "storage", "databaseAdapter"]) {
      if ((entrypoints as readonly string[]).includes(registry)) {
        throw new Error(
          `plugin.toml: \`[triggers].crons\` cannot be combined with the \`${registry}\` entrypoint; put the schedule in a separate integration plugin`,
        );
      }
    }
  }
  validateQueues(m);
  validateKvNamespaces(m);
  for (const [i, consumer] of consumers.entries()) {
    const type = typeof consumer.type === "string" ? consumer.type : "";
    if (!type.trim()) {
      throw new Error(`plugin.toml: events.consumers[${i}].type is required`);
    }
    if (!EVENT_TYPE_RE.test(type)) {
      throw new Error(
        `plugin.toml: events.consumers[${i}].type \`${type}\` must be snake_case \`[a-z][a-z0-9_]*\``,
      );
    }
    validateMessageRetention(`events.consumers[${i}]`, consumer.message_retention_period);
  }
  const produced = new Set<string>();
  for (const [i, producer] of (m.events?.producers ?? []).entries()) {
    const type = typeof producer.type === "string" ? producer.type : "";
    if (!EVENT_TYPE_RE.test(type)) {
      throw new Error(
        `plugin.toml: events.producers[${i}].type \`${type}\` must be snake_case \`[a-z][a-z0-9_]*\``,
      );
    }
    if (produced.has(type)) {
      throw new Error(`plugin.toml: events.producers[${i}].type \`${type}\` is duplicated`);
    }
    produced.add(type);
    if (HOST_EVENT_TYPES.has(producer.type)) {
      throw new Error(
        `plugin.toml: events.producers[${i}].type \`${producer.type}\` is emitted by the host ` +
          "(after a source's acquire handler returns); plugins may consume it but not publish it",
      );
    }
  }
  if (m.cli != null && !seen.has("cli")) {
    throw new Error('plugin.toml: `[cli]` requires `"cli"` in `entrypoints`');
  }
  if ((m.oidc?.clients?.length ?? 0) > 0 && !seen.has("oidc")) {
    throw new Error('plugin.toml: `[[oidc.clients]]` requires `"oidc"` in `entrypoints`');
  }
  const bindings = new Set<string>();
  for (const db of m.databases ?? []) {
    const name = db.binding;
    if (!/^[A-Z][A-Z0-9_]*$/.test(name) || name.length > 32) {
      throw new Error(
        `plugin.toml: [[databases]] binding \`${name}\` must be \`[A-Z][A-Z0-9_]*\` and at most 32 chars`,
      );
    }
    if (name === "NETWORK") {
      throw new Error(
        "plugin.toml: [[databases]] binding `NETWORK` is reserved for the network grant",
      );
    }
    if (bindings.has(name)) {
      throw new Error(`plugin.toml: [[databases]] binding \`${name}\` is duplicated`);
    }
    bindings.add(name);
  }
  const reserved = new Set(["CONFIG", "SECRETS", "EVENTS", "WORK_FS", "OAUTH", "SOCKETS"]);
  // Row index, not the binding text. The text can be an oauth name, and an
  // error that includes it is logged as clear text.
  const named: Array<[string, string, number]> = [];
  if (m.secrets) named.push(["secrets", m.secrets.binding || "SECRETS", 0]);
  if (m.work_fs) named.push(["work_fs", m.work_fs.binding || "WORK_FS", 0]);
  if (m.oauth) named.push(["oauth", m.oauth.binding || "OAUTH", 0]);
  if (m.sockets) named.push(["sockets", m.sockets.binding || "SOCKETS", 0]);
  if ((m.notifications?.types ?? []).length > 0) named.push(["notifications", "NOTIFY", 0]);
  if ((m.ai?.calls ?? []).length > 0) named.push(["ai", "AI", 0]);
  for (const [index, kv] of (m.kv_namespaces ?? []).entries()) {
    named.push(["kv_namespaces", kv.binding || "KV", index]);
  }
  for (const [index, producer] of (m.events?.producers ?? []).entries()) {
    named.push(["events.producers", producer.binding || "EVENTS", index]);
  }
  for (const [index, producer] of (m.queues?.producers ?? []).entries()) {
    named.push(["queues.producers", String(producer.binding ?? ""), index]);
  }
  for (const [table, name, index] of named) {
    if (!/^[A-Z][A-Z0-9_]*$/.test(name) || name.length > 32) {
      throw new Error(`plugin.toml: [${table}][${index}] binding must be \`[A-Z][A-Z0-9_]*\``);
    }
    if ((table === "kv_namespaces" || table === "queues.producers") && reserved.has(name)) {
      throw new Error(`plugin.toml: [${table}][${index}] binding collides with another binding`);
    }
    if (name === "CONFIG" || name === "NETWORK" || bindings.has(name)) {
      throw new Error(`plugin.toml: [${table}][${index}] binding collides with another binding`);
    }
  }
  for (const [table, name, index] of named) {
    if (table === "events.producers") continue;
    if (bindings.has(name)) {
      throw new Error(`plugin.toml: [${table}][${index}] binding collides with another binding`);
    }
    bindings.add(name);
  }
  // Producers may share one publisher binding, but `env.SOCKETS` is its own
  // property and cannot take a publisher's name, nor `EVENTS`, which the
  // generated `Env` always carries when the plugin publishes.
  const socketsName = named.find(([table]) => table === "sockets")?.[1];
  const producerNames = named
    .filter(([table]) => table === "events.producers")
    .map(([, name]) => name);
  if (
    socketsName !== undefined &&
    producerNames.length > 0 &&
    (socketsName === "EVENTS" || producerNames.includes(socketsName))
  ) {
    throw new Error("plugin.toml: [sockets][0] binding collides with another binding");
  }
}

/** Newest compatibility date the pinned workerd binary honors. */
export const WORKERD_PIN_COMPAT_DATE = "2026-11-13";

/** Author flags allowed on this pin. Required together for Python guests. */
export const PYTHON_COMPATIBILITY_FLAGS = [
  "python_workers",
  "disable_python_external_sdk",
] as const;

/** Date written into the workerd config, plus a fallback warning when clamped. */
export interface AppliedCompatibilityDate {
  /** Author date on or before the pin, otherwise {@link WORKERD_PIN_COMPAT_DATE}. */
  applied: string;
  /** Wrangler-style warning when `applied` was clamped. `null` otherwise. */
  warning: string | null;
}

/**
 * Rejects a compatibility date that is not a real `YYYY-MM-DD`.
 *
 * A date newer than the pin is still valid. Load falls back and warns; see
 * {@link applyAuthorCompatibilityDate}.
 *
 * @param date - `workerd.compatibility_date`.
 * @throws {Error} When the date is not a calendar day.
 */
export function validateAuthorCompatibilityDate(date: string): void {
  if (!isCalendarDate(date)) {
    throw new Error("plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD");
  }
}

/**
 * Resolves an author compatibility date against this Bookclerk release.
 *
 * workerd enables flags whose default-on date is on or before the date it is
 * given, and refuses a date newer than the one baked into the binary. Wrangler
 * warns and starts at the newest date that binary supports. Bookclerk host
 * surfaces must use `applied`, the same date the isolate runs.
 *
 * @param date - `workerd.compatibility_date`.
 * @returns The date to pass to workerd, and a warning when it was clamped.
 * @throws {Error} When the date is not a calendar day.
 */
export function applyAuthorCompatibilityDate(date: string): AppliedCompatibilityDate {
  validateAuthorCompatibilityDate(date);
  if (date > WORKERD_PIN_COMPAT_DATE) {
    return {
      applied: WORKERD_PIN_COMPAT_DATE,
      warning: compatibilityDateFallbackWarning(date),
    };
  }
  return { applied: date, warning: null };
}

function compatibilityDateFallbackWarning(requested: string): string {
  return (
    `The latest compatibility date supported by the installed Bookclerk workerd runtime is "${WORKERD_PIN_COMPAT_DATE}",\n` +
    `but you've requested "${requested}". Falling back to "${WORKERD_PIN_COMPAT_DATE}"...\n` +
    "Features enabled by your requested compatibility date may not be available.\n" +
    "Upgrade Bookclerk to a release that supports this date."
  );
}

/**
 * Enforces the Python flag pair. Does not insert missing flags.
 *
 * @param flags - Author `compatibility_flags`.
 * @param python - True when the manifest declares a Python module. A
 * disk-only `.py` file is not enough.
 * @throws {Error} When a flag is outside the allowlist or the pair is wrong.
 */
export function validateAuthorCompatibilityFlags(flags: readonly string[], python: boolean): void {
  for (const flag of flags) {
    if (flag === "experimental") {
      throw new Error("plugin.toml: workerd.compatibility_flags `experimental` is host-only");
    }
    if (!(PYTHON_COMPATIBILITY_FLAGS as readonly string[]).includes(flag)) {
      throw new Error(`plugin.toml: workerd.compatibility_flags \`${flag}\` is not allowed`);
    }
  }
  const has = (name: string) => flags.includes(name);
  const both = PYTHON_COMPATIBILITY_FLAGS.every((flag) => has(flag));
  const any = PYTHON_COMPATIBILITY_FLAGS.some((flag) => has(flag));
  if (python && !both) {
    throw new Error(
      "plugin.toml: workerd.compatibility_flags must include python_workers and disable_python_external_sdk when the guest is Python",
    );
  }
  if (any && !python) {
    throw new Error("plugin.toml: workerd.compatibility_flags require a Python module");
  }
}

/**
 * True when the manifest declares Python (main `.py`, `type = "python"`, or a
 * `.py` load-set key). An explicit `path` is that key; `name` is used only
 * when `path` is omitted. Flags are not evidence.
 *
 * @param m - Manifest under validation.
 * @returns Whether Python flags and Pyodide consent hosts apply.
 */
export function declaresPython(m: Manifest): boolean {
  if ((m.runtime ?? "native") !== "workerd") return false;
  const main = m.workerd?.main_module ?? "";
  if (main.toLowerCase().endsWith(".py")) return true;
  for (const mod of m.modules ?? []) {
    const type = (mod.type ?? "js").toLowerCase();
    const path = (mod.path || mod.name || "").toLowerCase();
    if (type === "python" || path.endsWith(".py")) return true;
  }
  return false;
}

/**
 * Spawn/load refusal for a declared-but-unbuilt surface.
 *
 * Every manifest surface is implemented today (`[[kv_namespaces]]` and
 * `[queues]` included); the hook stays so a future table can refuse load
 * with "not implemented yet" without touching the callers.
 *
 * @param _m - Manifest to inspect.
 * @returns The refusal, or `null` when load may continue.
 */
export function unimplementedSurface(_m: Manifest): string | null {
  return null;
}

/**
 * Checks main and `[[modules]]` extensions. Unimplemented extensions say
 * "not implemented yet". A type that disagrees with the extension is rejected.
 *
 * @param mainModule - `[workerd].main_module`.
 * @param modules - `[[modules]]` rows.
 * @throws {Error} When a file would not be embedded or its type disagrees.
 */
export function validateModuleDeclarations(
  mainModule: string,
  modules: ReadonlyArray<{ name: string; path?: string; type?: string }>,
): void {
  if (embedClass(mainModule) == null) {
    throw new Error(`plugin.toml: workerd.main_module \`${mainModule}\` is not implemented yet`);
  }
  for (const mod of modules) {
    const path = mod.path || mod.name;
    const embed = embedClass(path);
    if (embed == null) {
      throw new Error(`plugin.toml: [[modules]] \`${path}\` is not implemented yet`);
    }
    const type = mod.type ?? "js";
    if (!moduleTypeMatches(embed, type)) {
      throw new Error(
        `plugin.toml: [[modules]] \`${path}\` type \`${type}\` does not match the file extension`,
      );
    }
  }
}

/**
 * Relative key a modules-directory walk uses for a `[[modules]]` path.
 *
 * `.` segments are dropped, then a leading modules-dir prefix is removed, so
 * `./modules/index.js` and `modules/pkg/./echo.wasm` match walk keys.
 * `..` segments are left in place.
 *
 * @param modulesDir - `[workerd].modules_dir`.
 * @param raw - Author path or name.
 * @returns Slash-separated key with a leading modules-dir prefix removed.
 */
export function moduleLoadKey(modulesDir: string, raw: string): string {
  const key = normalizeDotSegments(raw);
  const dir = normalizeDotSegments(trimSlashes(modulesDir));
  if (dir && key.startsWith(`${dir}/`)) return key.slice(dir.length + 1);
  return key;
}

function trimSlashes(value: string): string {
  let start = 0;
  let end = value.length;
  while (start < end && value[start] === "/") start += 1;
  while (end > start && value[end - 1] === "/") end -= 1;
  return value.slice(start, end);
}

function normalizeDotSegments(raw: string): string {
  const slash = raw.replace(/\\/g, "/");
  const parts: string[] = [];
  let start = 0;
  for (let i = 0; i <= slash.length; i += 1) {
    if (i !== slash.length && slash[i] !== "/") continue;
    const segment = slash.slice(start, i);
    if (segment !== ".") parts.push(segment);
    start = i + 1;
  }
  return parts.join("/");
}

/**
 * True when the walk embeds this filename.
 *
 * @param path - Module path or filename.
 * @returns Whether the extension is `.js`, `.mjs`, `.py`, `.wasm`, or `.json`.
 */
export function workerdModuleIsEmbedded(path: string): boolean {
  return embedClass(path) != null;
}

function embedClass(path: string): "js" | "python" | "wasm" | "json" | null {
  const lower = path.toLowerCase();
  if (lower.endsWith(".py")) return "python";
  if (lower.endsWith(".wasm")) return "wasm";
  if (lower.endsWith(".mjs") || lower.endsWith(".js")) return "js";
  if (lower.endsWith(".json")) return "json";
  return null;
}

function moduleTypeMatches(embed: string, moduleType: string): boolean {
  const kind = moduleType.trim().toLowerCase();
  if (embed === "python") return kind === "python";
  if (embed === "wasm") return kind === "wasm";
  if (embed === "json") return kind === "json";
  if (embed === "js")
    return kind === "js" || kind === "javascript" || kind === "esm" || kind === "esmodule";
  return false;
}

function isCalendarDate(text: string): boolean {
  if (!/^\d{4}-\d{2}-\d{2}$/.test(text)) return false;
  const year = Number(text.slice(0, 4));
  const month = Number(text.slice(5, 7));
  const day = Number(text.slice(8, 10));
  const max = daysInMonth(year, month);
  return max != null && day >= 1 && day <= max;
}

function daysInMonth(year: number, month: number): number | null {
  if (month === 2) return isLeapYear(year) ? 29 : 28;
  if ([1, 3, 5, 7, 8, 10, 12].includes(month)) return 31;
  if ([4, 6, 9, 11].includes(month)) return 30;
  return null;
}

function isLeapYear(year: number): boolean {
  return (year % 4 === 0 && year % 100 !== 0) || year % 400 === 0;
}

/** Most `[triggers] crons` entries one manifest may declare (host `MAX_CRON_TRIGGERS`). */
export const MAX_CRON_TRIGGERS = 8;

/** Longest cron source in bytes (ABI `maxCronBytes`). */
export { MAX_CRON_BYTES };

/** One cron field item: `*`, a value or a range, optionally with `/step`. */
const CRON_ITEM = /^(\*|[0-9A-Za-z]+(-[0-9A-Za-z]+)?)(\/[0-9]{1,2})?$/;

/**
 * Checks the shape of `[triggers] crons`: at most {@link MAX_CRON_TRIGGERS}
 * ASCII entries of five fields, each a comma list of `*`, values, ranges and
 * `/step`, with no duplicates once whitespace is normalized. Value ranges,
 * names and never-firing schedules are checked by the host when the package
 * is installed (`CronSchedule::parse`).
 *
 * @param crons - The `crons` array as parsed from TOML.
 * @throws Error naming the first entry that is malformed.
 */
export function validateCrons(crons: unknown): void {
  if (!Array.isArray(crons)) {
    throw new Error("plugin.toml: triggers.crons must be an array of strings");
  }
  if (crons.length > MAX_CRON_TRIGGERS) {
    throw new Error(
      `plugin.toml: triggers.crons has ${crons.length} entries; at most ${MAX_CRON_TRIGGERS} are allowed`,
    );
  }
  const seen = new Set<string>();
  for (const [i, raw] of crons.entries()) {
    if (
      typeof raw !== "string" ||
      raw.length > MAX_CRON_BYTES ||
      !/^[\x20-\x7e\t\n\f\r]*$/.test(raw)
    ) {
      throw new Error(
        `plugin.toml: triggers.crons[${i}] must be an ASCII string of at most ${MAX_CRON_BYTES} bytes`,
      );
    }
    const fields = raw
      .trim()
      .split(/\s+/)
      .filter((f) => f.length > 0);
    if (fields.length !== 5) {
      throw new Error(
        `plugin.toml: triggers.crons[${i}] \`${raw}\` needs 5 fields (minute hour day-of-month month day-of-week)`,
      );
    }
    for (const field of fields) {
      if (!field.split(",").every((item) => CRON_ITEM.test(item))) {
        throw new Error(`plugin.toml: triggers.crons[${i}] field \`${field}\` is not valid cron`);
      }
    }
    const normalized = fields.join(" ");
    if (seen.has(normalized)) {
      throw new Error(`plugin.toml: triggers.crons[${i}] \`${normalized}\` is duplicated`);
    }
    seen.add(normalized);
  }
}
