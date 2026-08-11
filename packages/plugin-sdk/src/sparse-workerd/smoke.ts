/**
 * Out-of-tree workerd plugin smoke: ensure → materialize → handshake + health.
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

async function postRpc(
  rpcUrl: string,
  body: { id: string | number; method: string; params?: unknown },
  token: string,
): Promise<unknown> {
  const res = await fetch(rpcUrl, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      Authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  if (!res.ok && res.status !== 400) {
    throw new Error(`bridge HTTP ${res.status}: ${text}`);
  }
  const value = JSON.parse(text) as {
    id?: unknown;
    result?: unknown;
    error?: { code?: string; message?: string };
  };
  if (value.error) {
    throw new Error(
      `RPC ${body.method} failed: ${value.error.code ?? "internal"}: ${value.error.message ?? "bridge error"}`,
    );
  }
  return value.result;
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
 * Smoke a workerd plugin directory without the Rust `bookclerk-workerd` binary.
 * Returns a human-readable success summary; throws on failure.
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
    const rpcUrl = `${base}/rpc`;
    const handshake = await postRpc(
      rpcUrl,
      {
        id: 1,
        method: "handshake",
        params: { apiVersion: 1, config: {} },
      },
      bridgeToken,
    );
    const health = await postRpc(
      rpcUrl,
      {
        id: 2,
        method: "health",
        params: {},
      },
      bridgeToken,
    );
    const detail = {
      plugin: manifest.id,
      listen: generated.listenAddr,
      handshake,
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
