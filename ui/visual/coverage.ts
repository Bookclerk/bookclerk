/**
 * Which web app components the visual suite renders.
 *
 * Before a story or app page loads, the test installs a stand-in for the
 * React DevTools hook, which React calls on every commit. After each commit
 * it walks the fiber tree and notes every component that put something on
 * the page (an element or text below it). A dialog that is mounted but
 * closed returns nothing and is not noted. The visual builds keep function
 * names (they are not minified), so a component is known by its name.
 *
 * Each test writes the names it saw to its output folder; the global
 * teardown (coverage-check.ts) fails the run when a component module under
 * `ui/src/components/` exports no component that rendered in any test.
 */
import { writeFileSync } from "node:fs";
import path from "node:path";
import type { Page, TestInfo } from "@playwright/test";

/** The web app package (ui/). */
export const UI = path.join(import.meta.dirname, "..");

/** The file each test writes its rendered component names to. */
export const COVERAGE_FILE = "rendered-components.json";

/**
 * Installs the DevTools hook stand-in; runs in the page before any script.
 * Self-contained: Playwright sends it to the page as source.
 */
function installHook(): void {
  type Fiber = {
    tag: number;
    type: unknown;
    child: Fiber | null;
    sibling: Fiber | null;
  };
  const seen = new Set<string>();
  // React's fiber tags for host elements and text (HostComponent,
  // HostText, HostHoistable, HostSingleton).
  const HOST = new Set([5, 6, 26, 27]);
  const nameOf = (type: unknown): string | null => {
    if (typeof type === "function") {
      const fn = type as { displayName?: string; name?: string };
      return fn.displayName || fn.name || null;
    }
    if (type && typeof type === "object") {
      const wrapped = type as { displayName?: string; render?: unknown; type?: unknown };
      return wrapped.displayName || nameOf(wrapped.render) || nameOf(wrapped.type);
    }
    return null;
  };
  // Post-order walk: whether a fiber's subtree holds a host node; every
  // component above one has rendered.
  const walk = (fiber: Fiber | null): boolean => {
    let any = false;
    for (let f = fiber; f; f = f.sibling) {
      const host = HOST.has(f.tag);
      const here = walk(f.child) || host;
      if (here && !host) {
        const name = nameOf(f.type);
        if (name) seen.add(name);
      }
      any = any || here;
    }
    return any;
  };
  const target = window as unknown as Record<string, unknown>;
  target.__VISUAL_RENDERED__ = seen;
  const renderers = new Map<number, unknown>();
  target.__REACT_DEVTOOLS_GLOBAL_HOOK__ = {
    supportsFiber: true,
    isDisabled: false,
    renderers,
    inject(renderer: unknown) {
      const id = renderers.size + 1;
      renderers.set(id, renderer);
      return id;
    },
    onCommitFiberRoot(_id: number, root: { current: Fiber }) {
      walk(root.current.child);
    },
    onCommitFiberUnmount() {},
    onPostCommitFiberRoot() {},
    checkDCE() {},
  };
}

/**
 * Starts noting which components a test's page renders.
 *
 * @param page - The browser page, before it navigates.
 */
export async function startRecording(page: Page): Promise<void> {
  await page.addInitScript(installHook);
}

/**
 * Writes the components the page rendered to the test's output folder, for
 * the teardown check.
 *
 * @param page - The browser page.
 * @param testInfo - The running test.
 */
export async function saveRecording(page: Page, testInfo: TestInfo): Promise<void> {
  const names = await page.evaluate(() => {
    const seen = (window as unknown as { __VISUAL_RENDERED__?: Set<string> }).__VISUAL_RENDERED__;
    return seen ? [...seen].sort() : [];
  });
  writeFileSync(testInfo.outputPath(COVERAGE_FILE), JSON.stringify(names));
}
