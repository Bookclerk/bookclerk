/**
 * Materialize Cap'n Proto workerd config + bridge assets (mirrors config.rs).
 */

import fs from "node:fs";
import path from "node:path";
import type { Manifest } from "../tools/validate.js";
import { packageRoot } from "./ensure.js";

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

Use: from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

Native stdio guests use the pip package's BookclerkPlugin +
BookclerkPluginGuest.serve instead.
"""
`;

export type MaterializeOptions = {
  /** Loopback listen port for the bridge socket. */
  listenPort: number;
  /** Optional HOST.notify reverse channel (`host:port`). Smoke omits this. */
  notifyAddr?: string | null;
  /** Override package root (tests). */
  sdkRoot?: string;
  /** Cap'n Proto output filename (default `.bookclerk-workerd-config.capnp`). */
  configName?: string;
};

export type GeneratedConfig = {
  configPath: string;
  listenAddr: string;
};

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

function collectModules(dir: string): string[] {
  const out: string[] = [];
  const walk = (d: string) => {
    for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
      const p = path.join(d, ent.name);
      if (ent.isDirectory()) {
        walk(p);
        continue;
      }
      if (!ent.isFile()) continue;
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
  walk(dir);
  out.sort();
  return out;
}

export function pluginGlobalOutbound(mode: string): "blocked" | "egress" {
  return mode === "outbound" ? "egress" : "blocked";
}

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
 * Materialize bridge assets + Cap'n Proto under `pluginRoot`.
 * `notifyAddr` may be omitted (smoke path).
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
  const sdkRoot = options.sdkRoot ?? packageRoot();
  const modulesDirName = workerd.modules_dir ?? "modules";
  const entrypoint = workerd.entrypoint ?? "default";
  const networkMode = manifest.capabilities?.network?.mode ?? "deny";
  const networkDomains = manifest.capabilities?.network?.domains ?? [];

  const bookclerkDir = path.join(pluginRoot, ".bookclerk");
  fs.mkdirSync(bookclerkDir, { recursive: true });
  for (const name of ["bridge.js", "egress.js", "host_stub.js"] as const) {
    const src = path.join(sdkRoot, "bridge", name);
    fs.copyFileSync(src, path.join(bookclerkDir, name));
  }

  const modulesDir = path.join(pluginRoot, modulesDirName);
  if (!fs.existsSync(modulesDir) || !fs.statSync(modulesDir).isDirectory()) {
    throw new Error(`modules dir missing: ${modulesDir}`);
  }
  const mainAbs = path.join(modulesDir, workerd.main_module);
  if (!fs.existsSync(mainAbs) || !fs.statSync(mainAbs).isFile()) {
    throw new Error(`main module missing: ${mainAbs}`);
  }

  let moduleFiles = collectModules(modulesDir);
  moduleFiles = moduleFiles.filter((p) => path.resolve(p) !== path.resolve(mainAbs));
  const ordered = [mainAbs, ...moduleFiles];

  const moduleEmbeds: string[] = [];
  let needsPython = false;
  let needsJs = false;
  const seenNames = new Set<string>();

  for (const filePath of ordered) {
    const rel = path
      .relative(pluginRoot, filePath)
      .split(path.sep)
      .join("/");
    const name = path
      .relative(modulesDir, filePath)
      .split(path.sep)
      .join("/");
    if (isLegacySdkEmbed(name)) continue;
    const { field, python } = moduleFieldFor(name);
    if (python) needsPython = true;
    else if (name.endsWith(".js") || name.endsWith(".mjs")) needsJs = true;
    seenNames.add(name);
    moduleEmbeds.push(
      `(name = "${escapeCapnp(name)}", ${field} = embed "${escapeCapnp(rel)}")`,
    );
  }

  if (needsJs) {
    const sdkJs = fs.readFileSync(
      path.join(sdkRoot, "embed", "bookclerk_plugin.js"),
      "utf8",
    );
    fs.writeFileSync(path.join(bookclerkDir, "sdk-workerd.js"), sdkJs);
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
    const pyCandidates = [
      path.join(sdkRoot, "..", "plugin-sdk-python", "src", "bookclerk_plugin_sdk", "workerd.py"),
      path.join(sdkRoot, "python-workerd.py"),
    ];
    const pySrc = pyCandidates.find((p) => fs.existsSync(p));
    if (!pySrc) {
      throw new Error(
        "Python workerd SDK (workerd.py) not found beside @bookclerk/plugin-sdk; " +
          "install bookclerk-plugin-sdk or use the Python smoke CLI for .py plugins",
      );
    }
    fs.copyFileSync(pySrc, path.join(bookclerkDir, "sdk-workerd.py"));
    fs.writeFileSync(path.join(bookclerkDir, "sdk-init.py"), SDK_PY_INIT);
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

  const entrypointBinding =
    entrypoint === "default"
      ? `(name = "PLUGIN", service = "plugin")`
      : `(name = "PLUGIN", service = (name = "plugin", entrypoint = "${escapeCapnp(entrypoint)}"))`;

  const listenAddr = `127.0.0.1:${options.listenPort}`;
  const pluginOutbound = pluginGlobalOutbound(networkMode);

  let notifyService = "";
  let hostBindings = "";
  if (options.notifyAddr) {
    notifyService = `    (name = "hostNotify", external = (address = "${escapeCapnp(options.notifyAddr)}", http = ())),`;
    hostBindings = `(name = "NOTIFY", service = "hostNotify")`;
  }

  const compatDate = escapeCapnp(workerd.compatibility_date);
  const config = `using Workerd = import "/workerd/workerd.capnp";

const bookclerkPlugin :Workerd.Config = (
  services = [
    (name = "internet", network = (allow = ["public"])),
    (name = "blocked", network = (allow = [])),
    (name = "host", worker = .hostWorker),
    (name = "egress", worker = .egressWorker),
    (name = "plugin", worker = .pluginWorker),
    (name = "bridge", worker = .bridgeWorker),
${notifyService}
  ],
  sockets = [
    (name = "rpc", address = "${listenAddr}", http = (), service = "bridge")
  ]
);

const hostWorker :Workerd.Worker = (
  modules = [
    (name = "host_stub.js", esModule = embed ".bookclerk/host_stub.js")
  ],
  compatibilityDate = "${compatDate}",
  
  bindings = [
    ${hostBindings}
  ],
  globalOutbound = "blocked",
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
  bindings = [
    (name = "HOST", service = "host"),
  ],
  globalOutbound = "${pluginOutbound}",
);

const bridgeWorker :Workerd.Worker = (
  modules = [
    (name = "bridge.js", esModule = embed ".bookclerk/bridge.js")
  ],
  compatibilityDate = "${compatDate}",
  
  bindings = [
    ${entrypointBinding}
  ],
  globalOutbound = "blocked",
);
`;

  const configName = options.configName ?? ".bookclerk-workerd-config.capnp";
  const configPath = path.join(pluginRoot, configName);
  fs.writeFileSync(configPath, config);
  return { configPath, listenAddr };
}
