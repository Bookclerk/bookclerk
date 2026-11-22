/**
 * Visual and accessibility check of every story in the static Storybook build.
 *
 * The story list comes from the build's `index.json`, so a new story is
 * covered without touching this file. Each story is checked in light and
 * dark at every viewport project.
 */
import { readFileSync } from "node:fs";
import path from "node:path";
import AxeBuilder from "@axe-core/playwright";
import { expect, test } from "@playwright/test";
import { saveRecording, startRecording } from "./coverage.ts";

/** One entry of Storybook's `index.json`. */
type IndexEntry = {
  /** Story id, such as `primitives-button--primary`. */
  id: string;
  /** `story` for a renderable story, `docs` for an autodocs page. */
  type: "story" | "docs";
  /** Tags the story carries; `skip-visual` opts a story out. */
  tags?: string[];
};

/** Themes every story is rendered in, matching the `theme` global. */
const THEMES = ["light", "dark"] as const;

/** WCAG 2.2 A and AA rule tags axe-core checks. */
const WCAG_TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

/**
 * Reads the story ids from the static build.
 *
 * @returns Every renderable story id, in index order.
 */
function storyIds(): string[] {
  const indexPath = path.join(import.meta.dirname, "..", "storybook-static", "index.json");
  const index = JSON.parse(readFileSync(indexPath, "utf8")) as {
    entries: Record<string, IndexEntry>;
  };
  return Object.values(index.entries)
    .filter((e) => e.type === "story" && !(e.tags ?? []).includes("skip-visual"))
    .map((e) => e.id);
}

for (const id of storyIds()) {
  for (const theme of THEMES) {
    test(`${id} (${theme})`, async ({ page }, testInfo) => {
      await startRecording(page);
      await page.goto(`/iframe.html?id=${id}&viewMode=story&globals=theme:${theme}`);
      // Rendered: the root has an element (a story may hold no text at all).
      await expect(page.locator("#storybook-root > *").first()).toBeAttached();
      await expect(page.locator("html")).toHaveClass(
        theme === "dark" ? /\bdark\b/ : /^(?!.*\bdark\b)/,
      );
      // Self-hosted fonts: wait for them so text never renders in a fallback.
      await page.evaluate(() => document.fonts.ready);
      // Storybook fades the story in; axe-core would otherwise measure contrast
      // mid-fade. Finish every running animation and transition first (an
      // endless one, such as a spinner, is left to the screenshot's own
      // animation freeze).
      await page.evaluate(() => {
        for (const animation of document.getAnimations()) {
          if (animation.effect?.getComputedTiming().endTime !== Infinity) animation.finish();
        }
      });

      const axe = await new AxeBuilder({ page })
        .include("#storybook-root")
        .withTags(WCAG_TAGS)
        .analyze();
      expect(
        axe.violations.map(
          (v) => `${v.id}: ${v.help} (${v.nodes.map((n) => n.target.join(" ")).join(", ")})`,
        ),
      ).toEqual([]);

      // The story's own box, not the whole viewport: the empty page background
      // would make up most of every image (and of the baselines repository).
      await expect(page.locator("#storybook-root")).toHaveScreenshot(`${id}--${theme}.png`);
      await saveRecording(page, testInfo);
    });
  }
}
