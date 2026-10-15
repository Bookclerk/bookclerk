/**
 * `plugin.toml` semantic validation (mirrors `bookclerk-plugin-manifest` rules).
 *
 * Used by `bookclerk-plugin check|fmt|package` and the sparse-workerd smoke path.
 */

/**
 * Parsed `plugin.toml` shape accepted by the TypeScript author tools.
 *
 * Field names match the TOML keys (snake_case), not the Workers RPC wire
 * (camelCase). See `docs/plugins.md` for the full manifest contract.
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
   * `databaseAdapter`, `remoteLibrary`, `cli`, `oidc`). Triggers for the
   * default entrypoint live under `[triggers]` / `[[events.consumers]]`.
   */
  entrypoints?: string[];
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
  /** `[triggers]` — command types the default entrypoint's `job(controller)` runs. */
  triggers?: { jobs?: string[] };
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
  kv_namespaces?: Array<{ binding?: string; namespace?: string; max_value_bytes?: number; max_keys?: number }>;
  /**
   * `[queues]` — Workers Queues: producers are `env` send bindings,
   * consumers are `queue(batch)` triggers on the host outbox.
   */
  queues?: QueuesToml;
  /** `[work_fs]` — scratch filesystem binding (default name `WORK_FS`). */
  work_fs?: { binding?: string };
  /** `[oauth]` — loopback OAuth callback binding (default name `OAUTH`). */
  oauth?: { binding?: string };
  /** Network policy the operator must consent to. */
  capabilities: {
    /** Egress policy (`deny` / `outbound`, plus hostname domains for workerd). */
    network: { mode: string; domains?: string[] };
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
};

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
  return name.length > 0 && name.length <= MAX_QUEUE_NAME_LEN && /^[a-z0-9][a-z0-9._-]*$/.test(name);
}

function validateQueueName(field: string, name: unknown): void {
  const text = typeof name === "string" ? name : "";
  if (!isValidQueueName(text)) {
    throw new Error(
      `plugin.toml: ${field} \`${text}\` must be \`[a-z0-9][a-z0-9._-]*\` and at most ${MAX_QUEUE_NAME_LEN} chars`,
    );
  }
  if (RESERVED_QUEUE_PREFIXES.some((prefix) => text.startsWith(prefix))) {
    throw new Error(`plugin.toml: ${field} \`${text}\` uses a host-reserved prefix (\`core.\`, \`host.\`)`);
  }
}

/**
 * Domain event types only the host emits (`book_acquired` is written by the
 * acquire pipeline once a source's acquire handler returns). Plugins consume
 * them but cannot declare them under `[[events.producers]]`.
 */
export const HOST_EVENT_TYPES: ReadonlySet<string> = new Set(["book_acquired"]);

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
      throw new Error(`plugin.toml: kv_namespaces[${i}].max_value_bytes must be 1..=${MAX_KV_NAMESPACE_VALUE_BYTES}`);
    }
    const maxKeys = row.max_keys ?? 10_000;
    if (!Number.isInteger(maxKeys) || maxKeys < 1 || maxKeys > MAX_KV_NAMESPACE_KEYS) {
      throw new Error(`plugin.toml: kv_namespaces[${i}].max_keys must be 1..=${MAX_KV_NAMESPACE_KEYS}`);
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
      throw new Error(`plugin.toml: queues.consumers[${i}].queue \`${consumer.queue}\` is duplicated`);
    }
    seen.add(consumer.queue);
    const batch = consumer.max_batch_size ?? 10;
    if (!Number.isInteger(batch) || batch < 1 || batch > 100) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_batch_size must be 1..=100`);
    }
    const timeout = consumer.max_batch_timeout ?? 5;
    if (!Number.isInteger(timeout) || timeout < 0 || timeout > 60) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_batch_timeout must be at most 60 seconds`);
    }
    const retries = consumer.max_retries ?? 3;
    if (!Number.isInteger(retries) || retries < 0 || retries > 100) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_retries must be at most 100`);
    }
    const delay = consumer.retry_delay ?? 0;
    if (!Number.isInteger(delay) || delay < 0 || delay > 43200) {
      throw new Error(`plugin.toml: queues.consumers[${i}].retry_delay must be at most 43200 seconds`);
    }
    if (consumer.max_concurrency != null && (!Number.isInteger(consumer.max_concurrency) || consumer.max_concurrency < 1)) {
      throw new Error(`plugin.toml: queues.consumers[${i}].max_concurrency must be at least 1`);
    }
    if (consumer.dead_letter_queue != null) {
      validateQueueName(`queues.consumers[${i}].dead_letter_queue`, consumer.dead_letter_queue);
      if (consumer.dead_letter_queue === consumer.queue) {
        throw new Error(`plugin.toml: queues.consumers[${i}].dead_letter_queue must differ from the consumed queue`);
      }
    }
  }
}

/** Value types accepted in `[[settings]] type`. */
export const SETTING_TYPES = ["string", "bool", "integer", "enum", "secret"] as const;

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
  /** Default applied when the instance document omits the key (not for secrets). */
  default?: string | number | boolean;
  /** When true, a spawn without a value fails. */
  required?: boolean;
  /** Optional help text. */
  description?: string;
};

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
    if (spec.type === "enum") {
      if (values.length === 0) {
        throw new Error(`plugin.toml: [[settings]] enum setting \`${key}\` must declare \`values\``);
      }
      const ids = new Set<string>();
      for (const value of values) {
        const id = String(value.id ?? "");
        if (!id || ids.has(id)) {
          throw new Error(`plugin.toml: [[settings]] enum setting \`${key}\` has an invalid or duplicated value id`);
        }
        ids.add(id);
      }
    } else if (values.length > 0) {
      throw new Error(`plugin.toml: [[settings]] setting \`${key}\` declares \`values\` but is not an enum`);
    }
    if (spec.type === "secret") {
      if (spec.default !== undefined) {
        throw new Error(`plugin.toml: [[settings]] secret setting \`${key}\` may not declare a default`);
      }
      if (!m.secrets) {
        throw new Error(`plugin.toml: [[settings]] secret setting \`${key}\` requires the \`[secrets]\` binding`);
      }
      continue;
    }
    if (m.vars === undefined) {
      throw new Error(`plugin.toml: [[settings]] setting \`${key}\` requires the \`[vars]\` binding`);
    }
    const def = spec.default;
    if (def === undefined) continue;
    const ok =
      spec.type === "bool"
        ? typeof def === "boolean"
        : spec.type === "integer"
          ? typeof def === "number" && Number.isInteger(def)
          : spec.type === "enum"
            ? typeof def === "string" && values.some((v) => v.id === def)
            : typeof def === "string";
    if (!ok) {
      throw new Error(`plugin.toml: [[settings]] default for setting \`${key}\` does not match its type`);
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
] as const;

const LOGO_EXTENSIONS = [
  ".png",
  ".jpg",
  ".jpeg",
  ".gif",
  ".webp",
  ".svg",
  ".ico",
] as const;

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
    throw new Error(
      `plugin id \`${id}\` must not have leading or trailing whitespace`,
    );
  }
  if (id.length < 2 || id.length > 32) {
    throw new Error(`plugin id \`${id}\` must be 2–32 characters`);
  }
  if (![...id].every((c) => /[a-z0-9_]/.test(c))) {
    throw new Error(
      `plugin id \`${id}\` must be lowercase ascii letters, digits, or \`_\``,
    );
  }
  if (id.startsWith("_") || id.endsWith("_") || id.includes("__")) {
    throw new Error(
      `plugin id \`${id}\` must not start/end with \`_\` or contain \`__\``,
    );
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

function validateParsedUrl(
  parsed: URL,
  original: string,
): { kind: "remote"; value: string } {
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
    throw new Error(
      "plugin.toml: `logo` URL must not include userinfo (user:pass@host)",
    );
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
    throw new Error(
      "plugin.toml: embedded `logo` must be a relative path under the plugin root",
    );
  }
  if (path.length >= 2 && path.charCodeAt(1) === 58 /* : */) {
    throw new Error(
      "plugin.toml: embedded `logo` must be a relative path (no drive letter)",
    );
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
  validateSurface(m);
  validateSettings(m);
  const runtime = m.runtime ?? "native";
  if (runtime === "native") {
    if (!m.command || !String(m.command).trim()) {
      throw new Error(
        'plugin.toml: `command` is required when runtime = "native"',
      );
    }
    if (
      m.capabilities?.network?.domains &&
      m.capabilities.network.domains.length > 0
    ) {
      throw new Error(
        'plugin.toml: capabilities.network.domains is only valid for runtime = "workerd" (native outbound is coarse jail networking with no hostname filter — omit domains)',
      );
    }
  } else if (runtime === "workerd") {
    if (!m.workerd) {
      throw new Error(
        'plugin.toml: `[workerd]` is required when runtime = "workerd"',
      );
    }
    if (
      typeof m.workerd.compatibility_date !== "string" ||
      !m.workerd.compatibility_date.trim()
    ) {
      throw new Error("plugin.toml: workerd.compatibility_date is required");
    }
    if (!m.workerd.main_module?.trim()) {
      throw new Error("plugin.toml: workerd.main_module is required");
    }
    validateAuthorCompatibilityDate(m.workerd.compatibility_date);
    validateModuleDeclarations(m.workerd.main_module, m.modules ?? []);
    validateAuthorCompatibilityFlags(
      m.workerd.compatibility_flags ?? [],
      declaresPython(m),
    );
    if (
      m.capabilities?.network?.mode === "outbound" &&
      (!m.capabilities.network.domains ||
        m.capabilities.network.domains.length === 0)
    ) {
      throw new Error(
        'plugin.toml: capabilities.network.domains is required when runtime = "workerd" and mode = "outbound"',
      );
    }
  } else {
    throw new Error(`plugin.toml: unknown runtime ${runtime}`);
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
  if (entrypoints.length === 0 && consumers.length === 0 && queueConsumers.length === 0 && jobs.length === 0) {
    throw new Error(
      "plugin.toml: declare at least one of `entrypoints`, `[[events.consumers]]`, `[[queues.consumers]]`, or `[triggers].jobs`",
    );
  }
  validateQueues(m);
  validateKvNamespaces(m);
  for (const consumer of consumers) {
    if (!consumer.type || !String(consumer.type).trim()) {
      throw new Error("plugin.toml: [[events.consumers]] `type` is required");
    }
  }
  for (const [i, producer] of (m.events?.producers ?? []).entries()) {
    if (!producer.type || !String(producer.type).trim()) {
      throw new Error("plugin.toml: [[events.producers]] `type` is required");
    }
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
    if (bindings.has(name)) {
      throw new Error(`plugin.toml: [[databases]] binding \`${name}\` is duplicated`);
    }
    bindings.add(name);
  }
  const reserved = new Set(["CONFIG", "SECRETS", "EVENTS", "WORK_FS", "OAUTH"]);
  // Row index, not the binding text. The text can be an oauth name, and an
  // error that includes it is logged as clear text.
  const named: Array<[string, string, number]> = [];
  if (m.secrets) named.push(["secrets", m.secrets.binding || "SECRETS", 0]);
  if (m.work_fs) named.push(["work_fs", m.work_fs.binding || "WORK_FS", 0]);
  if (m.oauth) named.push(["oauth", m.oauth.binding || "OAUTH", 0]);
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
      throw new Error(
        `plugin.toml: [${table}][${index}] binding must be \`[A-Z][A-Z0-9_]*\``,
      );
    }
    if ((table === "kv_namespaces" || table === "queues.producers") && reserved.has(name)) {
      throw new Error(
        `plugin.toml: [${table}][${index}] binding collides with another binding`,
      );
    }
    if (name === "CONFIG" || bindings.has(name)) {
      throw new Error(
        `plugin.toml: [${table}][${index}] binding collides with another binding`,
      );
    }
  }
  for (const [table, name, index] of named) {
    if (table === "events.producers") continue;
    if (bindings.has(name)) {
      throw new Error(
        `plugin.toml: [${table}][${index}] binding collides with another binding`,
      );
    }
    bindings.add(name);
  }
}

/** Newest compatibility date the pinned workerd binary honors. */
export const WORKERD_PIN_COMPAT_DATE = "2026-08-01";

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
    throw new Error(
      "plugin.toml: workerd.compatibility_date must be a calendar YYYY-MM-DD",
    );
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
export function applyAuthorCompatibilityDate(
  date: string,
): AppliedCompatibilityDate {
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
export function validateAuthorCompatibilityFlags(
  flags: readonly string[],
  python: boolean,
): void {
  for (const flag of flags) {
    if (flag === "experimental") {
      throw new Error(
        "plugin.toml: workerd.compatibility_flags `experimental` is host-only",
      );
    }
    if (!(PYTHON_COMPATIBILITY_FLAGS as readonly string[]).includes(flag)) {
      throw new Error(
        `plugin.toml: workerd.compatibility_flags \`${flag}\` is not allowed`,
      );
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
    throw new Error(
      "plugin.toml: workerd.compatibility_flags require a Python module",
    );
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
    throw new Error(
      `plugin.toml: workerd.main_module \`${mainModule}\` is not implemented yet`,
    );
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
  if (embed === "js") return kind === "js" || kind === "javascript" || kind === "esm" || kind === "esmodule";
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
