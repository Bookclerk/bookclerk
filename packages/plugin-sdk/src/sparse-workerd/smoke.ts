/**
 * Out-of-tree workerd plugin smoke: ensure → materialize → describe + health.
 *
 * Spawns the pinned `workerd` binary against a materialised Cap'n Proto config
 * and exercises the bridge `/health`, `describe()`, and (for `storefront`,
 * `remoteLibrary` and `metadata` entrypoints) the entrypoint `health` method over
 * `POST /invoke`.
 */

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { randomBytes } from "node:crypto";
import { spawn, type ChildProcess } from "node:child_process";
import { parse as parseToml } from "smol-toml";
import {
  ContentSourceHealthParamsCodec,
  ContentSourceHealthResultsCodec,
  MetadataProviderHealthParamsCodec,
  MetadataProviderHealthResultsCodec,
  RemoteLibraryHealthParamsCodec,
  RemoteLibraryHealthResultsCodec,
  decodeMessage,
  encodeMessage,
  type ContentSourceHealthParams,
  type ContentSourceHealthResults,
  type StructCodec,
} from "../generated-wire.js";
import type { HealthOk } from "../generated.js";
import { validateManifest, type Manifest } from "../tools/validate.js";
import { materializeConfig } from "./config.js";
import {
  assertPathInside,
  defaultCacheDir,
  ensureWorkerd,
  validateSpawnExecutable,
} from "./ensure.js";

/** Cap'n interfaces whose `health` method the smoke probes. */
export type HealthInterface = "ContentSource" | "RemoteLibrary" | "MetadataProvider";

/** `health` params / results codecs per probed interface. */
const HEALTH_CODECS: Readonly<
  Record<
    HealthInterface,
    {
      readonly params: StructCodec<ContentSourceHealthParams>;
      readonly results: StructCodec<ContentSourceHealthResults>;
    }
  >
> = Object.freeze({
  ContentSource: {
    params: ContentSourceHealthParamsCodec,
    results: ContentSourceHealthResultsCodec,
  },
  RemoteLibrary: {
    params: RemoteLibraryHealthParamsCodec,
    results: RemoteLibraryHealthResultsCodec,
  },
  MetadataProvider: {
    params: MetadataProviderHealthParamsCodec,
    results: MetadataProviderHealthResultsCodec,
  },
});

/** Default Cap'n Proto config name written by {@link materializeConfig}. */
const WORKERD_SMOKE_CONFIG = "workerd-config.capnp";

async function freeLoopbackPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, "127.0.0.1", () => {
      const addr = server.address();
      if (!addr || typeof addr === "string") {
        server.close();
        reject(new Error("failed to allocate loopback port"));
        return;
      }
      const { port } = addr;
      server.close((err) => (err ? reject(err) : resolve(port)));
    });
    server.on("error", reject);
  });
}

async function waitForHealth(base: string, token: string, timeoutMs = 15_000): Promise<void> {
  const url = `${base}/health`;
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const res = await fetch(url, {
        headers: { Authorization: `Bearer ${token}` },
      });
      if (res.ok) return;
    } catch {
      // retry
    }
    if (Date.now() > deadline) {
      throw new Error(`timeout waiting for ${url}`);
    }
    await new Promise((r) => setTimeout(r, 50));
  }
}

async function postJson(url: string, body: unknown, token: string): Promise<unknown> {
  const res = await fetch(url, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(body ?? {}),
  });
  const text = await res.text();
  if (!res.ok && res.status !== 400) {
    throw new Error(`bridge HTTP ${res.status}: ${text}`);
  }
  const value = JSON.parse(text) as {
    error?: { code?: string; message?: string };
  };
  if (value.error) {
    throw new Error(
      `POST ${url} failed: ${value.error.code ?? "internal"}: ${value.error.message ?? "bridge error"}`,
    );
  }
  return value;
}

/** Entrypoints with a `health` method, in probe order, and their interfaces. */
const HEALTH_ENTRYPOINTS: readonly (readonly [string, HealthInterface])[] = [
  ["storefront", "ContentSource"],
  ["remoteLibrary", "RemoteLibrary"],
  ["metadata", "MetadataProvider"],
];

/**
 * Interfaces to probe for `health`, from a manifest's entrypoints.
 *
 * @param entrypoints The manifest's `entrypoints` list.
 * @returns Every interface whose entrypoint the plugin exports, in probe
 *   order; empty when none has a health probe (the default entrypoint's
 *   event / job triggers have none).
 */
export function healthInterfaces(entrypoints: readonly string[]): HealthInterface[] {
  return HEALTH_ENTRYPOINTS.filter(([name]) => entrypoints.includes(name)).map(
    ([, iface]) => iface,
  );
}

/**
 * Returns a `HealthOk` reply, or throws when it reports not ready.
 *
 * @param iface Interface the reply came from, for the error message.
 * @param health The decoded `HealthOk` value.
 * @returns `health` unchanged when its `ok` is true.
 * @throws {Error} When `ok` is false, with the guest's detail.
 */
export function requireHealthy(iface: HealthInterface, health: HealthOk): HealthOk {
  if (!health.ok) {
    throw new Error(`${iface}.health reported not ready: ${health.detail || "no detail"}`);
  }
  return health;
}

/**
 * `POST /invoke` health probe: the `health$Params` Cap'n message for the
 * entrypoint's interface, decoded as its `health$Results` reply union.
 *
 * @param base Bridge base URL (`http://host:port`).
 * @param iface Cap'n interface that owns the `health` method.
 * @param token Bridge bearer token.
 * @returns The `HealthOk` value of the reply's `ok` arm (`err` arms throw).
 */
async function invokeHealth(
  base: string,
  iface: HealthInterface,
  token: string,
): Promise<HealthOk> {
  const { params, results } = HEALTH_CODECS[iface];
  const res = await fetch(`${base}/invoke`, {
    method: "POST",
    headers: {
      "content-type": "application/x-capnp",
      Authorization: `Bearer ${token}`,
      "x-bookclerk-interface": iface,
      "x-bookclerk-method": "health",
      "x-bookclerk-context": JSON.stringify({ invocation: { id: "smoke" } }),
    },
    body: encodeMessage(params, {}),
  });
  if (!res.ok) {
    throw new Error(`bridge HTTP ${res.status}: ${await res.text()}`);
  }
  const reply = decodeMessage(results, new Uint8Array(await res.arrayBuffer())).result;
  if (reply.kind === "err") {
    throw new Error(`${iface}.health failed: ${reply.value.code}: ${reply.value.message}`);
  }
  return reply.value;
}

function loadManifest(pluginDir: string): Manifest {
  const root = path.resolve(pluginDir);
  const tomlPath = assertPathInside(root, "plugin.toml");
  if (!fs.existsSync(tomlPath)) {
    throw new Error(`missing plugin.toml in ${root}`);
  }
  const m = parseToml(fs.readFileSync(tomlPath, "utf8")) as Manifest;
  validateManifest(m);
  return m;
}

function killChild(child: ChildProcess): void {
  if (child.exitCode !== null || child.signalCode !== null) return;
  try {
    child.kill("SIGTERM");
  } catch {
    // ignore
  }
  setTimeout(() => {
    if (child.exitCode === null && child.signalCode === null) {
      try {
        child.kill("SIGKILL");
      } catch {
        // ignore
      }
    }
  }, 2000).unref?.();
}

/**
 * Smokes a workerd plugin directory without the Rust `bookclerk-workerd` binary.
 *
 * @param pluginDir - Plugin root with `runtime = "workerd"`.
 * @returns Human-readable success summary including describe/health JSON.
 * @throws {Error} When the runtime is not workerd, workerd fails to start, or
 *   bridge calls fail.
 *
 * @example
 * ```ts
 * console.log(await runSmoke("./my-plugin"));
 * ```
 */
export async function runSmoke(pluginDir: string): Promise<string> {
  const root = path.resolve(pluginDir);
  const manifest = loadManifest(root);
  const runtime = manifest.runtime ?? "native";
  if (runtime !== "workerd") {
    throw new Error(`smoke requires runtime = "workerd" (got ${JSON.stringify(runtime)})`);
  }

  const workerdBin = await ensureWorkerd(defaultCacheDir());
  const port = await freeLoopbackPort();
  const bridgeToken = randomBytes(32).toString("hex");
  const generated = materializeConfig(root, manifest, {
    listenPort: port,
    bridgeToken,
  });
  const base = `http://${generated.listenAddr}`;
  const validatedBin = validateSpawnExecutable(workerdBin);
  const binBase = path.basename(validatedBin);
  if (binBase !== "workerd" && binBase !== "workerd.exe") {
    throw new Error(`expected workerd binary, got ${binBase}`);
  }
  const validatedConfig = validateSpawnExecutable(generated.configPath, generated.stateDir);
  if (path.basename(validatedConfig) !== WORKERD_SMOKE_CONFIG) {
    throw new Error(`expected ${WORKERD_SMOKE_CONFIG}, got ${path.basename(validatedConfig)}`);
  }
  const importPath = validateSpawnExecutable(generated.importPath);
  // Literal argv: workerd serve + config under session cwd; import-path = RO plugin root.
  const child = spawn("workerd", ["serve", WORKERD_SMOKE_CONFIG, `--import-path=${importPath}`], {
    cwd: generated.stateDir,
    stdio: ["ignore", "pipe", "pipe"],
    shell: false,
    env: {
      ...process.env,
      BOOKCLERK_PLUGIN_ROOT: root,
      PATH: `${path.dirname(validatedBin)}${path.delimiter}${process.env.PATH ?? ""}`,
    },
  });

  const logs: string[] = [];
  const onChunk = (buf: Buffer) => {
    const line = buf.toString("utf8").trimEnd();
    if (line) {
      logs.push(line);
      console.error(`workerd: ${line}`);
    }
  };
  child.stdout?.on("data", onChunk);
  child.stderr?.on("data", onChunk);

  try {
    await waitForHealth(base, bridgeToken);
    const describe = await postJson(`${base}/describe`, {}, bridgeToken);
    // `health` is a method of the storefront, remoteLibrary and metadata
    // entrypoints; the default entrypoint (event/job triggers) has no health
    // probe.
    const health: Partial<Record<HealthInterface, HealthOk>> = {};
    for (const iface of healthInterfaces(manifest.entrypoints ?? [])) {
      health[iface] = requireHealthy(iface, await invokeHealth(base, iface, bridgeToken));
    }
    const detail = {
      plugin: manifest.id,
      listen: generated.listenAddr,
      describe,
      health,
    };
    return `smoke ok ${manifest.id}\n${JSON.stringify(detail, null, 2)}`;
  } catch (err) {
    if (logs.length) {
      console.error(`workerd logs (${logs.length} chunks) before failure`);
    }
    throw err;
  } finally {
    killChild(child);
    await new Promise<void>((resolve) => {
      if (child.exitCode !== null || child.signalCode !== null) {
        resolve();
        return;
      }
      child.once("exit", () => resolve());
      setTimeout(() => resolve(), 3000).unref?.();
    });
  }
}
