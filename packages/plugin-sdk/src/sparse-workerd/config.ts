/**
 * Materializes Cap'n Proto workerd config + bridge assets (mirrors `config.rs`).
 *
 * Writes `.bookclerk/` bridge scripts and a Cap'n Proto config that wires the
 * author plugin worker, the first-party adapter isolate (`PLUGIN`,
 * `PLUGIN_<ENTRYPOINT>`, `PLUGIN_DESCRIBE`, `BRIDGE_TOKEN`), egress filter,
 * host stub, and HTTP bridge socket. The bridge talks only to the adapter.
 */

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import type { Manifest } from "../tools/validate.js";
import { assertPathInside, refuseSymlinkPath, packageRoot, ensureDirUnder, writeFileUnder, copyFileUnder } from "./ensure.js";

/**
 * Require a relative manifest path (`modules_dir`, `main_module`).
 *
 * Multiple components are valid (`dist/modules`, `nested/main.js`). A `..`
 * sequence inside a filename (`edition..2.js`) is not traversal. Parent,
 * absolute, empty, and NUL paths are rejected. Callers still contain the
 * joined path with {@link assertPathInside} and {@link refuseSymlinkPath}.
 *
 * @param value - Candidate relative path from the manifest or options.
 * @param label - Field name used in error messages.
 * @returns Normalized relative path using `/` separators.
 * @throws {Error} When `value` is empty, absolute, or contains a parent component.
 */
function relativeManifestPath(value: string, label: string): string {
  if (!value || value.includes("\0")) {
    throw new Error(`${label} is empty or contains NUL`);
  }
  const normalized = value.replace(/\\/g, "/");
  if (normalized.startsWith("/") || /^[A-Za-z]:/.test(normalized)) {
    throw new Error(`${label} must be a relative path: ${value}`);
  }
  const parts = normalized.split("/").filter((part) => part !== "" && part !== ".");
  if (parts.length === 0) {
    throw new Error(`${label} is empty: ${value}`);
  }
  for (const part of parts) {
    if (part === "..") {
      throw new Error(`${label} must not contain parent components: ${value}`);
    }
  }
  return parts.join("/");
}

/**
 * Require one filename (no separators). `edition..2.js` is allowed.
 *
 * @param value - Candidate filename.
 * @param label - Field name used in error messages.
 * @returns The validated filename.
 * @throws {Error} When `value` is empty, a separator path, `.`, or `..`.
 */
function singleFileName(value: string, label: string): string {
  if (
    !value ||
    value.includes("\0") ||
    value.includes("/") ||
    value.includes("\\") ||
    value === "." ||
    value === ".."
  ) {
    throw new Error(`${label} must be a single path component: ${value}`);
  }
  return value;
}
const SDK_JS_MODULE_NAMES = [
  "@bookclerk/plugin-sdk/workerd",
  "@bookclerk/plugin-sdk",
] as const;
const SDK_PY_WORKERD_MODULE = "bookclerk_plugin_sdk/workerd.py";
const SDK_PY_INIT_MODULE = "bookclerk_plugin_sdk/__init__.py";
const PYODIDE_EGRESS_HOSTS = [
  "cdn.jsdelivr.net",
  "pypi.org",
  "files.pythonhosted.org",
] as const;

const SDK_PY_INIT = `"""Bookclerk plugin SDK (workerd isolate).

Use: from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, js

Native guests use Rust serve() / PluginWorker instead.
"""
`;

/** First-party adapter isolate module (mirrors `ADAPTER_JS` in `config.rs`). */
const ADAPTER_JS = `import { wrapPluginFromBinding } from "@bookclerk/plugin-sdk/workerd";
export default wrapPluginFromBinding();
`;

/**
 * `(binding, exported class)` the adapter receives per manifest entrypoint
 * (mirrors `entrypoint_binding` in `config.rs`).
 */
const ENTRYPOINT_SERVICE_BINDINGS: Readonly<Record<string, readonly [string, string]>> =
  Object.freeze({
    storefront: ["PLUGIN_STOREFRONT", "Storefront"],
    storage: ["PLUGIN_STORAGE", "Storage"],
    databaseAdapter: ["PLUGIN_DATABASE_ADAPTER", "DatabaseAdapter"],
    remoteLibrary: ["PLUGIN_REMOTE_LIBRARY", "RemoteLibrary"],
    cli: ["PLUGIN_CLI", "Cli"],
    oidc: ["PLUGIN_OIDC", "Oidc"],
  });

/**
 * Typed capability declaration a manifest implies (mirrors Rust
 * `PluginManifest::capabilities`). The adapter's `describe()` returns it
 * verbatim so the host's widening check is plain equality.
 *
 * @param m - Validated manifest.
 * @returns Wire-shaped `PluginCapabilities` object.
 */
export function manifestCapabilities(m: Manifest): {
  entrypoints: string[];
  consumes: Array<{ eventType: string; schemaVersions: number[]; supportsSuspend: boolean }>;
  produces: string[];
  jobs: string[];
  databases: string[];
  bindings: string[];
} {
  const bindings: string[] = [];
  if (m.vars !== undefined) bindings.push("CONFIG");
  if (m.secrets) bindings.push(m.secrets.binding ?? "SECRETS");
  if (m.work_fs) bindings.push(m.work_fs.binding ?? "WORK_FS");
  if (m.oauth) bindings.push(m.oauth.binding ?? "OAUTH");
  for (const kv of m.kv_namespaces ?? []) bindings.push(kv.binding ?? "KV");
  for (const producer of m.events?.producers ?? []) {
    const name = producer.binding || "EVENTS";
    if (!bindings.includes(name)) bindings.push(name);
  }
  return {
    entrypoints: [...(m.entrypoints ?? [])],
    consumes: (m.events?.consumers ?? []).map((c) => ({
      eventType: c.type,
      schemaVersions: c.schema_versions ?? [1],
      supportsSuspend: c.supports_suspend ?? false,
    })),
    produces: (m.events?.producers ?? []).map((p) => p.type),
    jobs: [...(m.triggers?.jobs ?? [])],
    databases: (m.databases ?? []).map((d) => d.binding),
    bindings,
  };
}

/**
 * `PLUGIN_DESCRIBE` JSON the adapter answers `describe()` from (mirrors
 * `manifest_describe_json` in `config.rs`).
 *
 * @param m - Validated manifest.
 * @returns Serialized wire-shaped `PluginDescribe` projection.
 */
export function manifestDescribeJson(m: Manifest): string {
  const cli =
    m.cli && typeof m.cli === "object"
      ? (m.cli as { commands?: unknown[] })
      : { commands: [] };
  return JSON.stringify({
    apiVersion: m.api_version,
    id: m.id,
    displayName: m.name ?? "",
    rpcFeatures: ["rpc.scalarLimits", "rpc.streams"],
    capabilities: manifestCapabilities(m),
    cli: { commands: cli.commands ?? [] },
  });
}

/**
 * Options for {@link materializeConfig}.
 */
export type MaterializeOptions = {
  /** Loopback listen port for the HTTP bridge socket (`127.0.0.1:<port>`). */
  listenPort: number;
  /**
   * Per-isolate bearer for `/rpc` and `/health` (`BRIDGE_TOKEN`).
   *
   * Required — generate once per smoke/isolate and send on every bridge request.
   */
  bridgeToken: string;
  /**
   * Absolute path to the `@bookclerk/plugin-sdk` package root when it differs from
   * the default (used by unit tests that vendor a fixture package tree).
   */
  sdkRoot?: string;
  /**
   * Existing session directory for generated embeds (default: allocate under
   * `$TMPDIR`). Cap'n Proto + `.bookclerk/` are written here — never under the
   * tainted plugin install root.
   */
  stateDir?: string;
  /** Cap'n Proto output filename under the session dir (default `workerd-config.capnp`). */
  configName?: string;
};

/**
 * Paths produced by {@link materializeConfig}.
 */
export type GeneratedConfig = {
  /** Absolute path to the Cap'n Proto config file (under {@link stateDir}). */
  configPath: string;
  /** Loopback listen address (`127.0.0.1:<port>`). */
  listenAddr: string;
  /** Writable session directory holding `.bookclerk/` + Cap'n Proto. */
  stateDir: string;
  /** Pass to `workerd serve --import-path` for `/modules/…` embeds. */
  importPath: string;
};

/**
 * Allocate a unique writable session directory for workerd generated embeds.
 *
 * Keys the leaf by a short hash of the plugin root plus a random nonce so
 * concurrent sessions cannot clobber Cap'n Proto. Prefer `$TMPDIR` / OS temp;
 * fall back to `.bookclerk-state` beside the plugin only when temp is unset.
 *
 * @param pluginRoot - Plugin install root (used only as an opaque id seed).
 * @returns Canonical absolute session directory.
 */
export function allocateWorkerdStateDir(pluginRoot: string): string {
  const rootKey = createHash("sha256")
    .update(path.resolve(pluginRoot))
    .digest("hex")
    .slice(0, 8);
  // Allocate via mkdtemp under the OS temp dir with a literal prefix so we never
  // mkdir a raw operator TMPDIR/plugin join (local threat-model path sink).
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `bc-w${rootKey}-`));
  try {
    fs.chmodSync(dir, 0o700);
  } catch {
    // Non-unix: chmod may be unsupported; directory is still uniquely allocated.
  }
  return path.resolve(dir);
}

function escapeCapnp(s: string): string {
  return s.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

function isLegacySdkEmbed(name: string): boolean {
  const n = name.replace(/\\/g, "/");
  return (
    n === "bookclerk_plugin.js" ||
    n === "bookclerk_plugin.py" ||
    n === "@bookclerk/plugin-sdk" ||
    n === "@bookclerk/plugin-sdk/workerd" ||
    n === "@bookclerk/plugin-sdk/workerd.js" ||
    n === "bookclerk_plugin_sdk/workerd.py" ||
    n === "bookclerk_plugin_sdk/db_value.py" ||
    n === "bookclerk_plugin_sdk/_abi.py" ||
    n === "bookclerk_plugin_sdk/guest_sql.py" ||
    n === "bookclerk_plugin_sdk/__init__.py"
  );
}

function moduleFieldFor(name: string): { field: string; python: boolean } {
  const lower = name.toLowerCase();
  if (lower.endsWith(".py")) return { field: "pythonModule", python: true };
  if (lower.endsWith(".wasm")) return { field: "wasm", python: false };
  if (lower.endsWith(".js") || lower.endsWith(".mjs")) {
    return { field: "esModule", python: false };
  }
  if (lower.endsWith(".json")) return { field: "json", python: false };
  if (lower.endsWith(".txt") || lower.endsWith(".md")) {
    return { field: "text", python: false };
  }
  throw new Error(
    `unsupported workerd module type for \`${name}\` (use .js/.mjs/.py/.wasm/.json)`,
  );
}

function collectModules(dir: string, pluginRoot: string): string[] {
  refuseSymlinkPath(pluginRoot, dir);
  const root = path.resolve(dir);
  const out: string[] = [];
  const walk = (d: string) => {
    for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
      const joined = path.join(d, ent.name);
      if (ent.isSymbolicLink()) {
        throw new Error(`refusing symlink in workerd modules tree: ${joined}`);
      }
      const p = assertPathInside(root, path.relative(root, joined));
      if (ent.isDirectory()) {
        walk(p);
        continue;
      }
      if (!ent.isFile()) {
        throw new Error(`refusing unsupported modules entry type: ${joined}`);
      }
      const lower = ent.name.toLowerCase();
      if (
        lower.endsWith(".js") ||
        lower.endsWith(".mjs") ||
        lower.endsWith(".py") ||
        lower.endsWith(".wasm") ||
        lower.endsWith(".json")
      ) {
        out.push(p);
      }
    }
  };
  walk(root);
  out.sort();
  return out;
}

/**
 * Maps a manifest network mode to the plugin worker's `globalOutbound`.
 *
 * @param mode - Manifest `capabilities.network.mode` (`outbound` or deny-like).
 * @returns Cap'n Proto outbound service name (`egress` or `blocked`).
 */
export function pluginGlobalOutbound(mode: string): "blocked" | "egress" {
  return mode === "outbound" ? "egress" : "blocked";
}

/**
 * Builds the egress allowlist, appending Pyodide hosts when needed.
 *
 * @param needsPython - Whether the plugin embeds Python modules.
 * @param mode - Manifest network mode (`outbound` enables Pyodide hosts).
 * @param base - Domains declared in `capabilities.network.domains`.
 * @returns Deduplicated domain list for the egress policy JSON.
 */
export function egressDomainsFor(
  needsPython: boolean,
  mode: string,
  base: string[],
): string[] {
  const domains = [...base];
  if (needsPython && mode === "outbound") {
    for (const host of PYODIDE_EGRESS_HOSTS) {
      if (!domains.some((d) => d.toLowerCase() === host.toLowerCase())) {
        domains.push(host);
      }
    }
  }
  return domains;
}

/**
 * Materializes bridge assets + Cap'n Proto under a host session directory.
 *
 * Copies bridge scripts into `stateDir/.bookclerk/`, embeds plugin modules via
 * Cap'n Proto `/modules/…` + `--import-path` (read-only install root), and
 * writes the Cap'n Proto config beside those embeds — never under the tainted
 * plugin install path.
 *
 * @param pluginRoot - Plugin directory containing `plugin.toml` and modules.
 * @param manifest - Validated workerd manifest.
 * @param options - Listen port and bridge token.
 * @returns Generated config path, listen address, session dir, and import path.
 * @throws {Error} When `[workerd]` is missing, modules are absent, or the
 *   bridge token is empty.
 */
export function materializeConfig(
  pluginRoot: string,
  manifest: Manifest,
  options: MaterializeOptions,
): GeneratedConfig {
  const workerd = manifest.workerd;
  if (!workerd) {
    throw new Error('missing [workerd] table');
  }
  const root = fs.realpathSync(path.resolve(pluginRoot));
  const sdkRoot = path.resolve(options.sdkRoot ?? packageRoot());
  const stateDir = options.stateDir
    ? fs.realpathSync(path.resolve(options.stateDir))
    : allocateWorkerdStateDir(root);
  const modulesDirName = relativeManifestPath(
    workerd.modules_dir ?? "modules",
    "modules_dir",
  );
  const mainModuleName = relativeManifestPath(workerd.main_module, "main_module");
  const entrypoint = workerd.entrypoint ?? "default";
  const networkMode = manifest.capabilities?.network?.mode ?? "deny";
  const networkDomains = manifest.capabilities?.network?.domains ?? [];

  const bookclerkDir = ensureDirUnder(stateDir, ".bookclerk");
  refuseSymlinkPath(stateDir, bookclerkDir);
  for (const name of ["bridge.js", "egress.js"] as const) {
    const src = assertPathInside(sdkRoot, path.join("bridge", name));
    copyFileUnder(bookclerkDir, name, src);
  }
  writeFileUnder(bookclerkDir, "adapter.js", ADAPTER_JS);

  const modulesDir = assertPathInside(root, modulesDirName);
  refuseSymlinkPath(root, modulesDir);
  if (
    !fs.existsSync(modulesDir) ||
    fs.lstatSync(modulesDir).isSymbolicLink() ||
    !fs.statSync(modulesDir).isDirectory()
  ) {
    throw new Error(`modules dir missing: ${modulesDir}`);
  }
  const mainAbs = assertPathInside(modulesDir, mainModuleName);
  refuseSymlinkPath(root, mainAbs);
  if (
    !fs.existsSync(mainAbs) ||
    fs.lstatSync(mainAbs).isSymbolicLink() ||
    !fs.statSync(mainAbs).isFile()
  ) {
    throw new Error(`main module missing: ${mainAbs}`);
  }

  let moduleFiles = collectModules(modulesDir, root);
  moduleFiles = moduleFiles.filter((p) => path.resolve(p) !== path.resolve(mainAbs));
  const ordered = [mainAbs, ...moduleFiles];

  const moduleEmbeds: string[] = [];
  let needsPython = false;
  let needsJs = false;
  const seenNames = new Set<string>();

  for (const filePath of ordered) {
    const name = path
      .relative(modulesDir, filePath)
      .split(path.sep)
      .join("/");
    // Cap'n Proto `/…` = import-path relative (same as Rust materialize).
    const embed = `/${modulesDirName}/${name}`;
    if (isLegacySdkEmbed(name)) continue;
    const { field, python } = moduleFieldFor(name);
    if (python) needsPython = true;
    else if (name.endsWith(".js") || name.endsWith(".mjs")) needsJs = true;
    seenNames.add(name);
    moduleEmbeds.push(
      `(name = "${escapeCapnp(name)}", ${field} = embed "${escapeCapnp(embed)}")`,
    );
  }

  // The adapter isolate always needs the SDK embed; the author isolate gets it
  // when it has JS modules.
  const sdkJsPath = assertPathInside(sdkRoot, path.join("embed", "bookclerk_plugin.js"));
  const sdkJs = fs.readFileSync(sdkJsPath, "utf8");
  const writeGenerated = (name: string, contents: string | Buffer) => {
    writeFileUnder(bookclerkDir, name, contents);
  };
  const copyGenerated = (src: string, name: string) => {
    copyFileUnder(bookclerkDir, name, src);
  };

  writeGenerated("sdk-workerd.js", sdkJs);
  const adapterModules = [
    `(name = "adapter.js", esModule = embed ".bookclerk/adapter.js")`,
    ...SDK_JS_MODULE_NAMES.map(
      (modName) =>
        `(name = "${escapeCapnp(modName)}", esModule = embed ".bookclerk/sdk-workerd.js")`,
    ),
  ];
  if (needsJs) {
    for (const modName of SDK_JS_MODULE_NAMES) {
      if (seenNames.has(modName)) continue;
      moduleEmbeds.push(
        `(name = "${escapeCapnp(modName)}", esModule = embed ".bookclerk/sdk-workerd.js")`,
      );
      seenNames.add(modName);
    }
  }

  if (needsPython) {
    // Prefer sibling Python SDK workerd.py when developing in-repo; else fail clearly.
    const packagesRoot = path.resolve(sdkRoot, "..");
    const pyCandidates = [
      assertPathInside(
        packagesRoot,
        path.join(
          "plugin-sdk-python",
          "src",
          "bookclerk_plugin_sdk",
          "workerd.py",
        ),
      ),
      assertPathInside(sdkRoot, "python-workerd.py"),
    ];
    const pySrc = pyCandidates.find((p) => fs.existsSync(p));
    if (!pySrc) {
      throw new Error(
        "Python workerd SDK (workerd.py) not found beside @bookclerk/plugin-sdk; " +
          "install bookclerk-plugin-sdk or use the Python smoke CLI for .py plugins",
      );
    }
    copyGenerated(pySrc, "sdk-workerd.py");
    writeGenerated("sdk-init.py", SDK_PY_INIT);
    // Modules imported by workerd.py / db_value.py inside the isolate.
    const pySdkDir = path.dirname(pySrc);
    const pySiblings: Array<[string, string, string]> = [
      ["bookclerk_plugin_sdk/_abi.py", "_abi.py", "sdk-product-abi.py"],
      ["bookclerk_plugin_sdk/guest_sql.py", "guest_sql.py", "sdk-guest-sql.py"],
      ["bookclerk_plugin_sdk/db_value.py", "db_value.py", "sdk-db-value.py"],
    ];
    for (const [modName, srcName, embedFile] of pySiblings) {
      const src = assertPathInside(pySdkDir, srcName);
      if (!fs.existsSync(src)) {
        throw new Error(`Python workerd SDK module ${srcName} not found beside ${pySrc}`);
      }
      copyGenerated(src, embedFile);
      if (!seenNames.has(modName)) {
        moduleEmbeds.push(
          `(name = "${escapeCapnp(modName)}", pythonModule = embed ".bookclerk/${embedFile}")`,
        );
        seenNames.add(modName);
      }
    }
    if (!seenNames.has(SDK_PY_INIT_MODULE)) {
      moduleEmbeds.push(
        `(name = "${escapeCapnp(SDK_PY_INIT_MODULE)}", pythonModule = embed ".bookclerk/sdk-init.py")`,
      );
      seenNames.add(SDK_PY_INIT_MODULE);
    }
    if (!seenNames.has(SDK_PY_WORKERD_MODULE)) {
      moduleEmbeds.push(
        `(name = "${escapeCapnp(SDK_PY_WORKERD_MODULE)}", pythonModule = embed ".bookclerk/sdk-workerd.py")`,
      );
      seenNames.add(SDK_PY_WORKERD_MODULE);
    }
  }

  const flags = [...(workerd.compatibility_flags ?? [])];
  if (needsPython) {
    for (const required of ["python_workers", "disable_python_external_sdk"]) {
      if (!flags.includes(required)) flags.push(required);
    }
  }
  const flagsLine =
    flags.length === 0
      ? ""
      : `compatibilityFlags = [${flags.map((f) => `"${escapeCapnp(f)}"`).join(", ")}],`;

  const domains = egressDomainsFor(needsPython, networkMode, networkDomains);
  const policyJson = JSON.stringify({
    mode: networkMode === "outbound" ? "outbound" : "deny",
    domains,
    maxRedirects: 10,
  });
  const policyEscaped = escapeCapnp(policyJson);

  const authorBinding =
    entrypoint === "default"
      ? `(name = "PLUGIN", service = "plugin")`
      : `(name = "PLUGIN", service = (name = "plugin", entrypoint = "${escapeCapnp(entrypoint)}"))`;
  const namedEntrypointBindings = (manifest.entrypoints ?? [])
    .map((wire) => ENTRYPOINT_SERVICE_BINDINGS[wire])
    .filter((pair): pair is readonly [string, string] => pair !== undefined)
    .map(
      ([binding, cls]) =>
        `(name = "${binding}", service = (name = "plugin", entrypoint = "${cls}"))`,
    );
  const describeBinding = `(name = "PLUGIN_DESCRIBE", json = "${escapeCapnp(
    manifestDescribeJson(manifest),
  )}")`;

  const listenAddr = `127.0.0.1:${options.listenPort}`;
  const pluginOutbound = pluginGlobalOutbound(networkMode);
  const bridgeToken = options.bridgeToken;
  if (!bridgeToken) {
    throw new Error("bridgeToken is required");
  }
  const bridgeTokenBinding = `(name = "BRIDGE_TOKEN", text = "${escapeCapnp(bridgeToken)}")`;

  const compatDate = escapeCapnp(workerd.compatibility_date);
  const config = `using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "adapter", worker = .adapterWorker),
    (name = "bridge", worker = .bridgeWorker),
  ],
  sockets = [
    (name = "rpc", address = "${listenAddr}", http = (), service = "bridge")
  ]
);

const egressWorker :Workerd.Worker = (
  modules = [
    (name = "egress.js", esModule = embed ".bookclerk/egress.js")
  ],
  compatibilityDate = "${compatDate}",
  
  bindings = [
    (name = "EGRESS_POLICY", json = "${policyEscaped}")
  ],
  globalOutbound = "internet",
);

const pluginWorker :Workerd.Worker = (
  modules = [
    ${moduleEmbeds.join(",\n    ")}
  ],
  compatibilityDate = "${compatDate}",
  ${flagsLine}
  bindings = [],
  globalOutbound = "${pluginOutbound}",
);

const adapterWorker :Workerd.Worker = (
  modules = [
    ${adapterModules.join(",\n    ")}
  ],
  compatibilityDate = "${compatDate}",
  
  bindings = [
    ${[authorBinding, ...namedEntrypointBindings, describeBinding, bridgeTokenBinding].join(",\n    ")}
  ],
  globalOutbound = "blocked",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "${compatDate}",
  
  bindings = [
    (name = "PLUGIN", service = "adapter"),
    ${bridgeTokenBinding}
  ],
  globalOutbound = "blocked",
);
`;

  const configName = singleFileName(
    options.configName ?? "workerd-config.capnp",
    "configName",
  );
  const configPath = writeFileUnder(stateDir, configName, config);
  return {
    configPath,
    listenAddr,
    stateDir,
    importPath: root,
  };
}
