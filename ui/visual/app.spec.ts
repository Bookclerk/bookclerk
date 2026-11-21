/**
 * Visual and accessibility check of the web app's full pages.
 *
 * The config serves the app's production build (ui/visual-dist). There is no
 * server behind it: every `/api/` request is answered from the sample
 * library in app/fixtures.ts, each JSON answer checked against the daemon's
 * OpenAPI document first (app/openapi.ts), and the live-updates socket is
 * held open without messages. The clock is fixed, so relative times read the
 * same on every run. A request the samples do not answer, or an answer the
 * document does not allow, fails the test and is named.
 *
 * Each page is checked in light and dark (the app follows
 * `prefers-color-scheme` when its theme is "system") at every viewport
 * project: axe-core, then a full-page screenshot named
 * `app--<page>--<theme>.png`, beside the stories' baselines.
 */
import AxeBuilder from "@axe-core/playwright";
import { expect, test, type Page, type Route } from "@playwright/test";
import { NOW_MS, answer, type Scenario, type Viewer } from "./app/fixtures.ts";
import { saveRecording, startRecording } from "./coverage.ts";
import { documented, mismatches } from "./app/openapi.ts";
import { APP_ORIGIN } from "./servers.ts";

/** Themes every page is rendered in. */
const THEMES = ["light", "dark"] as const;

/** WCAG 2.2 A and AA rule tags axe-core checks, as for the stories. */
const WCAG_TAGS = ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa"];

/** One app page to check. */
type AppPage = {
  /** Page name, the screenshot's middle part. */
  name: string;
  /** Who is signed in. */
  viewer: Viewer;
  /** The path opened. */
  path: string;
  /** A variation of the sample library the page needs; default: none. */
  scenario?: Scenario;
  /** Steps after the page loads, such as opening a tab. */
  prepare?: (page: Page) => Promise<void>;
  /**
   * Whether to wait for the network to go quiet after `prepare`; false
   * for a page that holds a request open on purpose. Default: true.
   */
  idle?: boolean;
};

/**
 * Opens a Settings tab.
 *
 * @param name - The tab's label.
 * @returns A prepare step.
 */
function settingsTab(name: string): (page: Page) => Promise<void> {
  return async (page) => {
    const tab = page.getByRole("tab", { name, exact: true });
    await tab.click();
    await expect(tab).toHaveAttribute("aria-selected", "true");
  };
}

/** How long the API must go without a request before a page counts as settled. */
const SETTLE_MS = 500;

/** Every page checked. */
const PAGES: AppPage[] = [
  { name: "sign-in", viewer: "signed-out", path: "/" },
  { name: "discover", viewer: "owner", path: "/discover" },
  { name: "library", viewer: "owner", path: "/library" },
  {
    name: "book-details",
    viewer: "owner",
    path: "/library",
    prepare: async (page) => {
      await page
        .getByRole("button", { name: /Moby-Dick/ })
        .first()
        .click();
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  { name: "wishlist", viewer: "owner", path: "/wishlist" },
  { name: "accounts", viewer: "owner", path: "/accounts" },
  { name: "status", viewer: "owner", path: "/library-status" },
  { name: "settings-account", viewer: "owner", path: "/settings" },
  {
    name: "settings-users",
    viewer: "owner",
    path: "/settings",
    prepare: settingsTab("User Management"),
  },
  { name: "settings-sign-in", viewer: "owner", path: "/settings", prepare: settingsTab("Sign-in") },
  { name: "settings-nodes", viewer: "owner", path: "/settings", prepare: settingsTab("Nodes") },
  { name: "settings-library", viewer: "owner", path: "/settings", prepare: settingsTab("Library") },
  {
    name: "settings-branches",
    viewer: "owner",
    path: "/settings",
    prepare: settingsTab("Branches"),
  },
  {
    name: "settings-shared-branches",
    viewer: "owner",
    path: "/settings",
    prepare: settingsTab("Shared branches"),
  },
  {
    name: "settings-server",
    viewer: "operator",
    path: "/settings",
    prepare: settingsTab("Server Settings"),
  },
  {
    name: "settings-plugins",
    viewer: "operator",
    path: "/settings",
    prepare: settingsTab("Plugins"),
  },
  {
    name: "settings-output",
    viewer: "operator",
    path: "/settings",
    prepare: settingsTab("Output"),
  },
  // Pages and dialogs that open from a page, or appear only in some states.
  {
    name: "settings-plugin-detail",
    viewer: "operator",
    path: "/settings",
    prepare: async (page) => {
      await settingsTab("Plugins")(page);
      await page
        .getByRole("button", { name: /S3 storage/ })
        .first()
        .click();
      await expect(page.getByText("Placement", { exact: true }).first()).toBeVisible();
    },
  },
  {
    name: "plugin-consent",
    viewer: "operator",
    path: "/settings",
    prepare: async (page) => {
      await settingsTab("Plugins")(page);
      await page.getByRole("checkbox", { name: "Echo Books deployed" }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  {
    name: "settings-server-attention",
    viewer: "operator",
    path: "/settings",
    scenario: "attention",
    prepare: async (page) => {
      await settingsTab("Server Settings")(page);
      await page.getByRole("button", { name: "Save a recovery copy" }).click();
      await page.getByRole("button", { name: "View", exact: true }).click();
      await expect(page.getByRole("button", { name: "Hide", exact: true })).toBeVisible();
    },
  },
  {
    name: "settings-library-card",
    viewer: "owner",
    path: "/settings",
    prepare: async (page) => {
      await settingsTab("Library")(page);
      await page.getByRole("button", { name: "Sign in with a library card" }).click();
      await expect(
        page.getByRole("heading", { name: "Sign in with a library card" }),
      ).toBeVisible();
    },
  },
  {
    name: "change-picture",
    viewer: "owner",
    path: "/settings",
    prepare: async (page) => {
      await page.getByRole("button", { name: "Change profile picture" }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  {
    name: "add-branch",
    viewer: "owner",
    path: "/library",
    prepare: async (page) => {
      const direct = page.getByRole("button", { name: "Add a branch" });
      if (await direct.isVisible()) await direct.click();
      else {
        await page.getByRole("button", { name: "Open menu" }).click();
        await page.getByRole("button", { name: "Add a branch" }).click();
      }
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  { name: "library-other-libraries", viewer: "owner", path: "/library", scenario: "window" },
  {
    name: "discover-search",
    viewer: "owner",
    path: "/discover",
    prepare: async (page) => {
      const search = page.getByRole("combobox", { name: "Search store catalogs" });
      await search.fill("Stevenson");
      const sent = page.waitForRequest((r) => new URL(r.url()).pathname === "/api/discover/search");
      await search.press("Enter");
      // The sample answers any term, so check the app asked for this one.
      expect(new URL((await sent).url()).searchParams.get("q")).toBe("Stevenson");
      await expect(page.getByText("Kidnapped").first()).toBeVisible();
    },
  },
  {
    name: "discover-details",
    viewer: "owner",
    path: "/discover",
    prepare: async (page) => {
      await page
        .getByRole("button", { name: /^Middlemarch/ })
        .first()
        .click();
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  {
    name: "player",
    viewer: "owner",
    path: "/library",
    idle: false,
    prepare: startPlaying,
  },
  {
    name: "now-playing",
    viewer: "owner",
    path: "/library",
    idle: false,
    prepare: async (page) => {
      await startPlaying(page);
      await page.getByRole("button", { name: "Open the player" }).first().click();
      await expect(page.getByRole("dialog")).toBeVisible();
    },
  },
  { name: "sign-in-sso", viewer: "signed-out", path: "/", scenario: "sso" },
  { name: "second-factor", viewer: "owner", path: "/library", scenario: "enroll" },
  {
    name: "second-factor-app",
    viewer: "owner",
    path: "/library",
    scenario: "enroll",
    prepare: async (page) => {
      await page.getByRole("button", { name: "Set up authenticator" }).click();
      await expect(page.getByRole("img", { name: /QR/i })).toBeVisible();
    },
  },
  { name: "welcome", viewer: "operator", path: "/library", scenario: "fresh" },
  {
    name: "welcome-join",
    viewer: "operator",
    path: "/library",
    scenario: "fresh",
    prepare: async (page) => {
      await page.getByRole("button", { name: /^Join a library/ }).click();
      await expect(page.getByRole("heading", { name: "Join a library" })).toBeVisible();
    },
  },
  {
    name: "welcome-diagnostics",
    viewer: "operator",
    path: "/library",
    scenario: "fresh",
    prepare: async (page) => {
      await page.getByRole("button", { name: /^Start a new library/ }).click();
      await expect(page.getByRole("heading", { name: "Help improve Bookclerk" })).toBeVisible();
    },
  },
  {
    name: "join-approve",
    viewer: "owner",
    path: "/join/approve?client_id=branch&state=s1&code_challenge=c1",
  },
  {
    name: "clerk-approve",
    viewer: "owner",
    path: "/clerk/approve?client_id=clerk&state=s1&code_challenge=c1",
  },
  {
    name: "player-approve",
    viewer: "owner",
    path: "/apps/approve?client_id=player&state=s1&code_challenge=c1",
  },
  { name: "join-callback", viewer: "operator", path: "/join/callback?code=c1&state=s1" },
  { name: "clerk-callback", viewer: "operator", path: "/clerk/callback?code=c1&state=s1" },
];

/**
 * Starts the first Continue listening book and pauses it before its audio
 * arrives, so the player shows a fixed place. The audio request is held
 * unanswered, so the page never goes idle (the page sets `idle: false`).
 *
 * @param page - The browser page.
 */
async function startPlaying(page: Page): Promise<void> {
  await page.route(/\/api\/playback\/sessions\/[^/]+\/stream\//, () => {
    // Held: the audio never loads, so the place stays where it was.
  });
  const streaming = page.waitForRequest(/\/stream\//);
  await page
    .getByRole("button", { name: /^Continue / })
    .first()
    .click();
  await streaming;
  // While the book loads, the middle button (labelled Play, showing the
  // throbber) stops it from starting.
  const player = page.getByRole("region", { name: "Player" });
  await player.getByRole("button", { name: "Play", exact: true }).click();
  await expect(player.locator("svg.lucide-play")).toBeVisible();
}

/**
 * Answers the page's API requests from the samples and collects every
 * request they could not answer as documented.
 *
 * @param page - The browser page.
 * @param viewer - Who is signed in.
 * @param scenario - The variation of the sample library, if any.
 * @returns The problems found so far (filled in as requests arrive), and a
 *   wait for the page's API requests to settle.
 */
async function serveSamples(
  page: Page,
  viewer: Viewer,
  scenario?: Scenario,
): Promise<{ problems: string[]; settled: () => Promise<void> }> {
  const problems: string[] = [];
  let pending = 0;
  let last = Date.now();
  // Live updates: accept the socket and send nothing.
  await page.routeWebSocket(/\/api\/live$/, () => {});
  const handle = async (route: Route): Promise<void> => {
    const request = route.request();
    const url = new URL(request.url());
    const label = `${request.method()} ${url.pathname}`;
    let body: unknown = null;
    try {
      body = request.postDataJSON();
    } catch {
      // Not JSON.
    }
    const sample = answer(viewer, request.method(), url.pathname, body, scenario);
    if (!sample) {
      problems.push(`${label}: no sample answer`);
      await route.fulfill({ status: 404, json: { error: "no sample answer" } });
      return;
    }
    const status = sample.status ?? 200;
    if (sample.body) {
      await route.fulfill({ status, contentType: sample.body.contentType, body: sample.body.text });
      return;
    }
    // The value checked is the value sent: an omitted body is `{}`, an
    // explicit null stays null.
    const json = sample.json === undefined ? {} : sample.json;
    const op = documented(request.method(), url.pathname, status);
    if (!op) problems.push(`${label}: not in the OpenAPI document`);
    else if (op.schema === undefined) problems.push(`${label}: status ${status} is not documented`);
    else if (op.schema) problems.push(...mismatches(json, op.schema).map((m) => `${label}: ${m}`));
    await route.fulfill({ status, json });
  };
  await page.route(/\/api\//, async (route) => {
    pending += 1;
    last = Date.now();
    try {
      await handle(route);
    } finally {
      pending -= 1;
      last = Date.now();
    }
  });
  // A step's own requests (a tab's data) start after the document went
  // idle, so `networkidle` alone would not wait for them.
  const settled = async (): Promise<void> => {
    await expect
      .poll(() => pending === 0 && Date.now() - last >= SETTLE_MS, { intervals: [100] })
      .toBe(true);
  };
  return { problems, settled };
}

/**
 * The colors behind a color-contrast failure, so the message says what to
 * change.
 *
 * @param checks - The node's `any` check results.
 * @returns ` [fg on bg, ratio]`, or nothing for other rules.
 */
function contrast(checks: { id: string; data?: unknown }[]): string {
  const data = checks.find((c) => c.id === "color-contrast")?.data as
    { fgColor?: string; bgColor?: string; contrastRatio?: number } | undefined;
  if (!data?.fgColor) return "";
  return ` [${data.fgColor} on ${data.bgColor ?? "?"}, ${data.contrastRatio ?? "?"}:1]`;
}

for (const p of PAGES) {
  for (const theme of THEMES) {
    test(`app ${p.name} (${theme})`, async ({ page }, testInfo) => {
      const { problems, settled } = await serveSamples(page, p.viewer, p.scenario);
      await startRecording(page);
      await page.clock.setFixedTime(NOW_MS);
      await page.emulateMedia({ colorScheme: theme });
      await page.goto(APP_ORIGIN + p.path, { waitUntil: "networkidle" });
      try {
        await p.prepare?.(page);
      } catch (err) {
        // A step that found nothing often means a request went unanswered.
        throw new Error(`${String(err)}\nAPI problems so far: ${problems.join("; ") || "none"}`);
      }
      if (p.idle !== false) {
        await page.waitForLoadState("networkidle");
        await settled();
      }
      await page.evaluate(async () => {
        await document.fonts.ready;
        // Finish entrance animations (a dialog fading in) so axe and the
        // screenshot see the settled page; endless ones (spinners) stay.
        for (const animation of document.getAnimations()) {
          if (animation.effect?.getComputedTiming().endTime !== Infinity) animation.finish();
        }
        await Promise.all(
          [...document.images].map((img) => (img.complete ? null : img.decode().catch(() => null))),
        );
      });
      expect(problems, "API requests the samples answer as the OpenAPI document says").toEqual([]);

      const axe = await new AxeBuilder({ page }).withTags(WCAG_TAGS).analyze();
      expect(
        axe.violations.map(
          (v) =>
            `${v.id}: ${v.help} (${v.nodes.map((n) => `${n.target.join(" ")}${contrast(n.any)}`).join(", ")})`,
        ),
      ).toEqual([]);

      await expect(page).toHaveScreenshot(`app--${p.name}--${theme}.png`, { fullPage: true });
      await saveRecording(page, testInfo);
    });
  }
}
