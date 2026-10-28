// Vitest global setup for the `workers` project (vitest.config.ts): builds
// the site in Node, as the deploy does, and hands every file of dist/ and the
// docs shell to worker.test.ts, which runs inside the Workers runtime and
// cannot read the host's files itself.

import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

import type { TestProject } from "vitest/node";

import { build } from "../build.ts";
import type { DocsShell } from "../render.ts";
import { ENV } from "./build-env.ts";

/** The build worker.test.ts serves, as `inject("siteBuild")` returns it. */
export interface SiteBuild {
  /** Every file of dist/ by path from dist/ (`download.html`), base64-encoded. */
  files: Record<string, string>;
  /** The docs shell the build wrote. */
  shell: DocsShell;
}

declare module "vitest" {
  interface ProvidedContext {
    /** See SiteBuild. */
    siteBuild: SiteBuild;
  }
}

/**
 * Every file under a directory.
 *
 * @param dir - Directory to walk.
 * @returns Absolute paths, subdirectories included.
 */
function walk(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    return entry.isDirectory() ? walk(path) : [path];
  });
}

/**
 * Builds the site into a temporary directory and provides it.
 *
 * @param project - The project being set up.
 * @returns Teardown that removes the build.
 */
export default function setup(project: TestProject): () => void {
  const root = mkdtempSync(join(tmpdir(), "bookclerk-site-worker-"));
  const dist = join(root, "dist");
  build(ENV, dist, join(root, "docs-shell.json"));
  const files = Object.fromEntries(
    walk(dist).map((path) => [relative(dist, path), readFileSync(path).toString("base64")]),
  );
  const shell = JSON.parse(readFileSync(join(root, "docs-shell.json"), "utf8")) as DocsShell;
  project.provide("siteBuild", { files, shell });
  return () => rmSync(root, { recursive: true, force: true });
}
