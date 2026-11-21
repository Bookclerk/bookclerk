// Serves the built website locally the way the audit and the visual checks
// see it: tools/site/dist through the real Worker under `wrangler dev`
// (audit/entry.ts), with docs read from this checkout's docs/ through a local
// server instead of the docs repository, so nothing leaves the machine.
//
// audit.ts starts it in-process. The web UI's visual checks
// (ui/visual/playwright.config.ts) run it as a command and stop it with
// SIGTERM:
//
//   node serve.ts --port <port> [--log <file>] [--fixed-sample]
//
// --fixed-sample makes the homepage's catalog sample the same on every
// request, for screenshots. Run `npm run build` in tools/site first.

import { spawn } from "node:child_process";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import type { AddressInfo } from "node:net";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import { FIXTURE_PREFIX, UNAVAILABLE_KEY } from "./pages.ts";

/** This directory (tools/site/audit). */
const HERE = dirname(fileURLToPath(import.meta.url));
/** The site package (tools/site). */
export const SITE = join(HERE, "..");
/** This checkout's docs, served in place of the docs repository. */
const DOCS_DIR = join(SITE, "..", "..", "docs");
/** The docs fixture pages, served for keys under FIXTURE_PREFIX. */
const FIXTURE_DIR = join(HERE, "visual-docs");

/** A running local site. */
export interface ServedSite {
  /** The site origin, such as `http://127.0.0.1:6107`. */
  origin: string;
  /** Stops wrangler and the docs server. */
  stop: () => void;
}

/**
 * Maps a docs repository path (`<ref>/<key>.md`) to a file under docs/.
 *
 * @param docsDir - The docs directory.
 * @param pathname - The request path, `/<ref>/<key>.md`.
 * @returns The file, or null when the path is malformed or
 *   leaves docsDir.
 */
export function docsFile(docsDir: string, pathname: string): string | null {
  const key = /^\/[^/]+\/(.+\.md)$/.exec(decodeURIComponent(pathname))?.[1];
  if (key === undefined) return null;
  const root = resolve(docsDir);
  const file = resolve(root, key);
  if (relative(root, file).startsWith("..") || !file.startsWith(root + sep)) return null;
  return file;
}

/**
 * Picks the directory a docs repository path is read from: the fixture pages
 * for keys under FIXTURE_PREFIX (with the prefix removed from the path),
 * docs/ for everything else.
 *
 * @param pathname - The request path, `/<ref>/<key>.md`.
 * @returns The directory and the path to look up in it.
 */
export function docsRoot(pathname: string): { dir: string; path: string } {
  const fixture = /^(\/[^/]+\/)(.*)$/.exec(pathname);
  if (fixture?.[1] !== undefined && fixture[2]?.startsWith(FIXTURE_PREFIX))
    return { dir: FIXTURE_DIR, path: fixture[1] + fixture[2].slice(FIXTURE_PREFIX.length) };
  return { dir: DOCS_DIR, path: pathname };
}

/**
 * Starts the local docs server: `/<ref>/<key>.md` answers docs/<key>.md for
 * any ref (audit/visual-docs/ for keys under FIXTURE_PREFIX),
 * UNAVAILABLE_KEY answers 503, anything else 404.
 *
 * @returns Its origin and a stop function.
 */
export function serveDocs(): Promise<{ origin: string; close: () => void }> {
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? "/", "http://localhost").pathname;
    if (path.endsWith(`/${UNAVAILABLE_KEY}.md`)) {
      res.writeHead(503).end();
      return;
    }
    const root = docsRoot(path);
    const file = docsFile(root.dir, root.path);
    if (
      file === null ||
      !existsSync(file) ||
      !realpathSync(file).startsWith(realpathSync(root.dir) + sep)
    ) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, { "Content-Type": "text/markdown; charset=utf-8" }).end(readFileSync(file));
  });
  return new Promise((done) => {
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address() as AddressInfo;
      done({ origin: `http://127.0.0.1:${port}`, close: () => server.close() });
    });
  });
}

/**
 * Finds a free TCP port on 127.0.0.1.
 *
 * @returns The port.
 */
function freePort(): Promise<number> {
  return new Promise((done, fail) => {
    const probe = createNetServer();
    probe.on("error", fail);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address() as AddressInfo;
      probe.close(() => done(port));
    });
  });
}

/**
 * Writes the audit Worker's wrangler config: ../wrangler.toml with the audit
 * entry point, so the assets and routing cannot drift from the deployed site.
 * The audit-only `/__audit/` routes (audit/entry.ts) also run the Worker first.
 *
 * @returns The config file.
 * @throws {Error} When ../wrangler.toml no longer has the lines replaced.
 */
function writeWranglerConfig(): string {
  const base = readFileSync(join(SITE, "wrangler.toml"), "utf8");
  const replacements: [RegExp, string][] = [
    [/^name = .*$/m, 'name = "bookclerk-site-audit"'],
    [/^main = .*$/m, 'main = "entry.ts"'],
    [/^directory = .*$/m, 'directory = "../dist"'],
    [/^run_worker_first = \[/m, 'run_worker_first = ["/__audit/*", '],
  ];
  let config = base;
  for (const [pattern, line] of replacements) {
    if (!pattern.test(config))
      throw new Error(`tools/site/wrangler.toml has no line matching ${pattern}`);
    config = config.replace(pattern, line);
  }
  const file = join(HERE, "wrangler.generated.toml");
  writeFileSync(file, config);
  return file;
}

/** How serveSite runs the Worker. */
export interface ServeOptions {
  /** Where wrangler's output goes. */
  logFile: string;
  /** The port to listen on; default: a free one. */
  port?: number;
  /** The same homepage catalog sample on every request, for screenshots. */
  fixedSample?: boolean;
}

/**
 * Starts the local docs server and `wrangler dev` on the audit Worker, and
 * waits until it serves `/`.
 *
 * @param options - Log file, port and catalog sample.
 * @returns The site origin and a stop function.
 * @throws {Error} When tools/site/dist is missing, or wrangler exits or does
 *   not serve within two minutes.
 */
export async function serveSite(options: ServeOptions): Promise<ServedSite> {
  if (!existsSync(join(SITE, "dist", "index.html")))
    throw new Error("tools/site/dist is missing: run `npm run build` first");
  const docs = await serveDocs();
  const config = writeWranglerConfig();
  const port = options.port ?? (await freePort());
  const inspector = await freePort();
  const wrangler = join(SITE, "node_modules", ".bin", "wrangler");
  const args = [
    "dev",
    "--config",
    config,
    "--ip",
    "127.0.0.1",
    "--port",
    String(port),
    "--inspector-port",
    String(inspector),
    "--var",
    `AUDIT_DOCS_ORIGIN:${docs.origin}`,
  ];
  if (options.fixedSample) args.push("--var", "AUDIT_FIXED_SAMPLE:1");
  const child = spawn(wrangler, args, {
    cwd: HERE,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "true" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const { logFile } = options;
  mkdirSync(dirname(logFile), { recursive: true });
  // wrangler outlives a parent that dies without stopping it; take it along.
  const killChild = (): void => {
    child.kill("SIGTERM");
  };
  process.on("exit", killChild);
  child.stdout.on("data", (chunk) => appendFileSync(logFile, chunk));
  child.stderr.on("data", (chunk) => appendFileSync(logFile, chunk));
  let exited: number | null = null;
  child.on("exit", (code) => (exited = code ?? -1));
  const stop = (): void => {
    process.off("exit", killChild);
    child.kill("SIGTERM");
    docs.close();
  };
  const origin = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (exited !== null) {
      docs.close();
      throw new Error(`wrangler dev exited with ${exited}; see ${logFile}`);
    }
    try {
      const res = await fetch(`${origin}/`);
      if (res.ok) return { origin, stop };
    } catch {
      // Not listening yet.
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  stop();
  throw new Error(`wrangler dev did not serve / within two minutes; see ${logFile}`);
}

/**
 * Serves the site until SIGTERM or SIGINT.
 *
 * @returns Resolves once the site is up.
 */
async function main(): Promise<void> {
  const { values } = parseArgs({
    options: {
      port: { type: "string" },
      log: { type: "string", default: join(HERE, "wrangler.log") },
      "fixed-sample": { type: "boolean", default: false },
    },
  });
  const port = values.port === undefined ? undefined : Number(values.port);
  if (port !== undefined && !(Number.isInteger(port) && port > 0 && port < 65536))
    throw new Error(`--port must be a TCP port, got ${JSON.stringify(values.port)}`);
  const served = await serveSite({
    logFile: resolve(values.log),
    fixedSample: values["fixed-sample"],
    ...(port === undefined ? {} : { port }),
  });
  console.log(`serving ${served.origin}`);
  for (const signal of ["SIGTERM", "SIGINT"] as const)
    process.on(signal, () => {
      served.stop();
      process.exit(0);
    });
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((err: unknown) => {
    console.error(err instanceof Error ? err.stack : err);
    process.exit(1);
  });
}
