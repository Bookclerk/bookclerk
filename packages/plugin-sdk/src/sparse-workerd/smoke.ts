/**
 * Out-of-tree workerd plugin smoke: ensure → materialize → describe + health.
 *
 * Spawns the pinned `workerd` binary against a materialised Cap'n Proto config
 * and exercises the bridge `/health`, `describe()`, and (for content-source /
 * integration kinds) the role `health` route.
 */

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { randomBytes } from "node:crypto";
import { spawn, type ChildProcess } from "node:child_process";
import { parse as parseToml } from "smol-toml";
import { validateManifest, type Manifest } from "../tools/validate.js";
import { materializeConfig } from "./config.js";
import { defaultCacheDir, ensureWorkerd } from "./ensure.js";

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

async function waitForHealth(
  base: string,
  token: string,
  timeoutMs = 15_000,
): Promise<void> {
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

async function postJson(
  url: string,
  body: unknown,
  token: string,
): Promise<unknown> {
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

function loadManifest(pluginDir: string): Manifest {
  const tomlPath = path.join(pluginDir, "plugin.toml");
  if (!fs.existsSync(tomlPath)) {
    throw new Error(`missing plugin.toml in ${pluginDir}`);
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
    throw new Error(
      `smoke requires runtime = "workerd" (got ${JSON.stringify(runtime)})`,
    );
  }

  const workerdBin = await ensureWorkerd(defaultCacheDir());
  const port = await freeLoopbackPort();
  const bridgeToken = randomBytes(32).toString("hex");
  const generated = materializeConfig(root, manifest, {
    listenPort: port,
    notifyAddr: null,
    bridgeToken,
  });
  const base = `http://${generated.listenAddr}`;

  const child = spawn(workerdBin, ["serve", generated.configPath], {
    cwd: root,
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, BOOKCLERK_PLUGIN_ROOT: root },
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
    // Role `health` exists for content-source / integration kinds only.
    const healthPath =
      manifest.kind === "source"
        ? "/contentSource/health"
        : manifest.kind === "integration"
          ? "/integration/health"
          : null;
    const health = healthPath
      ? await postJson(`${base}${healthPath}`, {}, bridgeToken)
      : null;
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
