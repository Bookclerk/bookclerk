/**
 * plugin.toml semantic validation (mirrors bookclerk-plugin-manifest rules).
 */

export type Manifest = {
  api_version: number;
  id: string;
  name?: string;
  kind: string;
  version?: string;
  logo?: string;
  runtime?: string;
  command?: string;
  args?: string[];
  workerd?: {
    compatibility_date: string;
    compatibility_flags?: string[];
    main_module: string;
    modules_dir?: string;
    entrypoint?: string;
    limits?: { cpu_ms?: number; subrequests?: number };
  };
  modules?: Array<{ name: string; path: string; type?: string }>;
  capabilities: {
    network: { mode: string; domains?: string[] };
    bindings?: Record<string, boolean>;
    methods?: { list?: string[] };
  };
  cli?: unknown;
};

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
 * Strict plugin id grammar (mirrors Rust `validate_plugin_id`):
 * `[a-z0-9_]{2,32}` with no leading/trailing `_` and no `__`.
 * Ids are globally unique across kinds. Leading/trailing whitespace is
 * rejected (non-lossy), not trimmed.
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

/** Classify and validate `plugin.toml` `logo` (mirrors Rust `validate_logo`). */
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
  if (m.api_version !== 1) {
    throw new Error("plugin.toml: `api_version` must be 1");
  }
  if (m.logo != null) {
    validateLogo(String(m.logo));
  }
  const kind = m.kind;
  if (!["source", "integration", "output", "database"].includes(kind)) {
    throw new Error(`plugin.toml: invalid kind ${kind}`);
  }
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
    if (!m.workerd.compatibility_date?.trim()) {
      throw new Error("plugin.toml: workerd.compatibility_date is required");
    }
    if (!m.workerd.main_module?.trim()) {
      throw new Error("plugin.toml: workerd.main_module is required");
    }
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
