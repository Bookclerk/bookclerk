/**
 * Global teardown of the visual suite: fails the run when a web app
 * component never rendered in any story or app page (see coverage.ts).
 *
 * It runs only when `VISUAL_COVERAGE_CHECK=1` (the CI recording run sets
 * it), because a run of some tests (`-g`, one spec) leaves most components
 * unrendered on purpose.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";
import type { FullConfig } from "@playwright/test";
import { COVERAGE_FILE, UI } from "./coverage.ts";

/** Where the components live, relative to ui/. */
const COMPONENTS = "src/components";

/**
 * Every file under a folder.
 *
 * @param dir - The folder.
 * @returns Absolute paths.
 */
function walk(dir: string): string[] {
  let out: string[] = [];
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return out;
  }
  for (const name of names) {
    const full = path.join(dir, name);
    if (statSync(full).isDirectory()) out = out.concat(walk(full));
    else out.push(full);
  }
  return out;
}

/**
 * The component modules the suite must render: every `.tsx` file under
 * src/components except stories.
 *
 * @returns Paths relative to ui/.
 */
export function componentModules(): string[] {
  return walk(path.join(UI, COMPONENTS))
    .map((f) => path.relative(UI, f).split(path.sep).join("/"))
    .filter((f) => f.endsWith(".tsx") && !f.endsWith(".stories.tsx"))
    .sort();
}

/**
 * The component names a module's source exports, capitalized as React
 * components are: inline exports (`export function Name`, `export const
 * Name =`, `export default function Name`), export lists (`export { Name }`,
 * grouped or across lines, by the local binding a `Local as Name` renames)
 * and `export default Name;`. Re-exports from another module (`export { X }
 * from "./x"`) belong to that module and are skipped.
 *
 * @param text - The module's source.
 * @returns The component names, each once.
 */
export function exportedNames(text: string): string[] {
  const names = new Set<string>();
  const add = (name: string | undefined): void => {
    if (name && /^[A-Z]/.test(name) && !/^[A-Z][A-Z0-9_]*$/.test(name)) names.add(name);
  };
  for (const m of text.matchAll(
    /^export\s+(?:default\s+)?(?:function|const|class)\s+([A-Za-z_$][\w$]*)/gm,
  ))
    add(m[1]);
  for (const m of text.matchAll(/^export\s+default\s+([A-Za-z_$][\w$]*)\s*;/gm)) add(m[1]);
  for (const m of text.matchAll(/^export\s+(?:type\s+)?\{([^}]*)\}(\s*from\b)?/gm)) {
    if (m[2] || /^export\s+type\b/.test(m[0])) continue;
    for (const spec of (m[1] ?? "").split(",")) {
      const local = spec
        .trim()
        .replace(/^type\s+/, "")
        .split(/\s+as\s+/)[0];
      if (!spec.trim().startsWith("type ")) add(local);
    }
  }
  return [...names];
}

/**
 * The components a module exports (see `exportedNames`).
 *
 * @param file - The module, relative to ui/.
 * @returns The component names.
 */
export function exportedComponents(file: string): string[] {
  return exportedNames(readFileSync(path.join(UI, file), "utf8"));
}

/**
 * Checks that every component module rendered somewhere.
 *
 * @param config - The resolved Playwright config.
 * @throws {Error} Naming every exported component no story or app page
 *   rendered, and every module that exports none.
 */
export default function coverageCheck(config: FullConfig): void {
  if (process.env.VISUAL_COVERAGE_CHECK !== "1") return;
  const rendered = new Set<string>();
  const dirs = new Set(config.projects.map((p) => p.outputDir));
  for (const dir of dirs)
    for (const file of walk(dir))
      if (path.basename(file) === COVERAGE_FILE)
        for (const name of JSON.parse(readFileSync(file, "utf8")) as string[]) rendered.add(name);
  const modules = componentModules();
  const exported = modules.flatMap((m) =>
    exportedComponents(m).map((name) => ({ module: m, name })),
  );
  if (process.env.VISUAL_COVERAGE_REPORT)
    for (const e of exported)
      console.log(`${rendered.has(e.name) ? "yes" : " no"}  ${e.module}  ${e.name}`);
  const silent = modules.filter((m) => exportedComponents(m).length === 0);
  const missing = exported.filter((e) => !rendered.has(e.name));
  const problems = [
    ...missing.map((e) => `${e.module}: ${e.name} never rendered`),
    ...silent.map((m) => `${m}: exports no component (move non-UI code out of src/components)`),
  ];
  if (problems.length > 0)
    throw new Error(
      `${missing.length} of ${exported.length} web app components never rendered in a story or ` +
        `an app page (add a story, or an app page in visual/app.spec.ts that shows it):\n  ` +
        problems.join("\n  "),
    );
  console.log(`Visual coverage: all ${exported.length} web app components rendered.`);
}
