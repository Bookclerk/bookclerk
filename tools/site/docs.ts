// Reads the repository's docs/ for the site build: the sidebar titles, the
// sitemap and llms.txt / llms-full.txt come from the docs checked out beside
// the site (the newest release tag's docs once there is a release, else
// main's). The pages themselves are rendered by the Worker from the docs
// repository at request time (worker.ts); render.ts holds the rendering both
// share.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";

import { buildNav, renderDoc } from "./render.ts";
import type { NavGroup, RenderedDoc } from "./render.ts";

export { docMarkdownUrl, docUrl, GENERATED_TITLES, NAV, rewriteLink, siteMarkdown, slugify } from "./render.ts";

/**
 * Directories under docs/ that scripts/generate-api-docs.sh fills with
 * generated (gitignored) API reference. They are not docs; the published
 * reference comes from the docs repository.
 */
/** Every local doc rendered, and the sidebar built from them (see renderDocs). */
export interface RenderedDocs {
  /** The sidebar. */
  nav: NavGroup[];
  /** Each doc's key (path without `.md`) and its rendering. */
  docs: (RenderedDoc & { key: string })[];
}

export const GENERATED_DIRS: readonly string[] = ["api/rust", "api/typescript", "api/ui", "api/python"];

/**
 * Lists every Markdown file under a directory, skipping GENERATED_DIRS.
 *
 * @param root - The docs directory.
 * @returns Paths relative to `root`, POSIX separators, sorted.
 */
export function markdownFiles(root: string): string[] {
  const out: string[] = [];
  const walk = (dir: string): void => {
    for (const name of readdirSync(dir)) {
      const full = join(dir, name);
      if (GENERATED_DIRS.includes(relative(root, full).split(sep).join("/"))) continue;
      if (statSync(full).isDirectory()) walk(full);
      else if (name.endsWith(".md")) out.push(relative(root, full).split(sep).join("/"));
    }
  };
  walk(root);
  return out.sort();
}

/**
 * Reads and renders every doc.
 *
 * @param docsRoot - The repository's docs/ directory.
 * @param blobBase - GitHub URL prefix for repository files at the
 *   docs' ref, ending in `/blob/<ref>/`.
 * @param generatedTitles - Sidebar titles of
 *   generated pages, which are not in `docsRoot` but are published.
 * @returns The sidebar and each doc's title, summary, HTML body and Markdown copy
 *   (see render.ts).
 * @throws {Error} When the docs index (`README.md`) is missing.
 */
export function renderDocs(docsRoot: string, blobBase: string, generatedTitles: Readonly<Record<string, string>> = {}): RenderedDocs {
  const files = markdownFiles(docsRoot);
  const keys = new Set([...files.map((f) => f.slice(0, -".md".length)), ...Object.keys(generatedTitles)]);
  if (!keys.has("README")) throw new Error(`no README.md in ${docsRoot}`);
  const docs = files.map((file) => {
    const key = file.slice(0, -".md".length);
    return { key, ...renderDoc(readFileSync(join(docsRoot, file), "utf8"), key, { keys, blobBase }) };
  });
  const titles = new Map(docs.map((d) => [d.key, d.title]));
  for (const [key, title] of Object.entries(generatedTitles)) if (!titles.has(key)) titles.set(key, title);
  return { nav: buildNav(titles), docs };
}
