/**
 * Visual and accessibility check of the pages the Cloudflare Workers serve:
 * the website (tools/site) and the diagnostics collector
 * (tools/diagnostics-collector).
 *
 * The config's web servers run each Worker's local page server: the website
 * through its real Worker under `wrangler dev` (tools/site/audit/serve.ts),
 * the collector's page templates rendered from fixed sample data
 * (tools/diagnostics-collector/visual/serve.ts). Each page is checked in
 * light and dark (both follow `prefers-color-scheme`) at every viewport
 * project, and its whole length is compared with a baseline screenshot named
 * `<worker>--<page>--<theme>.png`, beside the stories' baselines.
 *
 * Each Worker's building blocks (its VISUAL_COMPONENTS: header, cards,
 * tables, forms, callouts and so on) are also checked on their own: axe-core
 * on just that element, and an element screenshot named
 * `<worker>--component--<name>--<theme>.png`, so a change to one block shows
 * as its own small image.
 */
import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page } from "@playwright/test";
import {
  VISUAL_COMPONENTS as COLLECTOR_COMPONENTS,
  VISUAL_PAGES as COLLECTOR_PAGES,
} from "../../tools/diagnostics-collector/visual/pages.ts";
import {
  VISUAL_COMPONENTS as SITE_COMPONENTS,
  VISUAL_PAGES as SITE_PAGES,
} from "../../tools/site/audit/pages.ts";
import { COLLECTOR_ORIGIN, SITE_ORIGIN } from "./servers.ts";

/** Themes every page is rendered in. */
const THEMES = ["light", "dark"] as const;

/** WCAG 2.2 A and AA rule tags axe-core checks, as for the stories. */
const WCAG_TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

/** One page to check. */
type WorkerPage = {
  /** Worker name, the screenshot's first part. */
  worker: "website" | "collector";
  /** Page name. */
  name: string;
  /** Full URL on the local server. */
  url: string;
  /** The status the page answers with. */
  status: number;
};

/** Every page of both Workers. */
const PAGES: WorkerPage[] = [
  ...SITE_PAGES.map((p) => ({
    worker: "website" as const,
    name: p.name,
    url: SITE_ORIGIN + p.path,
    status: p.status,
  })),
  ...COLLECTOR_PAGES.map((p) => ({
    worker: "collector" as const,
    name: p.name,
    url: COLLECTOR_ORIGIN + p.path,
    status: 200,
  })),
];

/** One building block to check. */
type WorkerComponent = {
  /** Worker name, the screenshot's first part. */
  worker: WorkerPage["worker"];
  /** Component name. */
  name: string;
  /** The page it is taken from. */
  page: WorkerPage;
  /** CSS selector for the element; the first match is used. */
  selector: string;
};

/**
 * Finds the page a component is taken from.
 *
 * @param worker - The Worker.
 * @param name - The page's name.
 * @returns The page.
 * @throws {Error} When the Worker has no page by that name.
 */
function pageNamed(worker: WorkerPage["worker"], name: string): WorkerPage {
  const page = PAGES.find((p) => p.worker === worker && p.name === name);
  if (!page) throw new Error(`${worker} has no visual page named ${name}`);
  return page;
}

/** Every building block of both Workers. */
const COMPONENTS: WorkerComponent[] = [
  ...SITE_COMPONENTS.map((c) => ({
    worker: "website" as const,
    name: c.name,
    page: pageNamed("website", c.page),
    selector: c.selector,
  })),
  ...COLLECTOR_COMPONENTS.map((c) => ({
    worker: "collector" as const,
    name: c.name,
    page: pageNamed("collector", c.page),
    selector: c.selector,
  })),
];

/**
 * Opens a page in a theme and waits until it is still: fonts loaded and
 * finite animations finished.
 *
 * @param page - The browser page.
 * @param p - The Worker page to open.
 * @param theme - The color scheme.
 */
async function open(page: Page, p: WorkerPage, theme: (typeof THEMES)[number]): Promise<void> {
  await page.emulateMedia({ colorScheme: theme });
  const response = await page.goto(p.url, { waitUntil: "load" });
  expect(response?.status()).toBe(p.status);
  // Self-hosted fonts: wait for them so text never renders in a fallback.
  await page.evaluate(() => document.fonts.ready);
  await page.evaluate(() => {
    for (const animation of document.getAnimations()) {
      if (animation.effect?.getComputedTiming().endTime !== Infinity) animation.finish();
    }
  });
}

/**
 * Runs axe-core and expects no violations.
 *
 * @param axe - The configured builder.
 */
async function expectNoViolations(axe: AxeBuilder): Promise<void> {
  const result = await axe.withTags(WCAG_TAGS).analyze();
  expect(
    result.violations.map(
      (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(" ")).join(", ")})`,
    ),
  ).toEqual([]);
}

for (const p of PAGES) {
  for (const theme of THEMES) {
    test(`${p.worker} ${p.name} (${theme})`, async ({ page }) => {
      await open(page, p, theme);
      await expectNoViolations(new AxeBuilder({ page }));
      await expect(page).toHaveScreenshot(`${p.worker}--${p.name}--${theme}.png`, {
        fullPage: true,
      });
    });
  }
}

for (const c of COMPONENTS) {
  for (const theme of THEMES) {
    test(`${c.worker} component ${c.name} (${theme})`, async ({ page }) => {
      await open(page, c.page, theme);
      const element = page.locator(c.selector).first();
      await expect(element).toBeVisible();
      await expectNoViolations(new AxeBuilder({ page }).include(c.selector));
      await expect(element).toHaveScreenshot(`${c.worker}--component--${c.name}--${theme}.png`);
    });
  }
}
