// Lighthouse and axe-core audit of the built bookclerk.app website, run by CI
// for pull requests that change the site (scripts/ci_plan, check
// `site_audit`). See docs/contributing/automation.md#website-checks.
//
// It serves tools/site/dist through the real Worker under `wrangler dev`
// (audit/entry.ts, with docs read from this checkout's docs/ through a local
// server instead of the docs repository, so nothing leaves the machine), then:
//
//   Lighthouse  every page, mobile and desktop; pages that answer 200 must
//               reach the PAGE thresholds, the 404 and docs-unavailable pages
//               only ERROR_PAGE (their SEO and best-practice misses, such as
//               the status code and noindex, are intended)
//   axe-core    every page, light and dark, at 1350 and 390 px: no violations
//               of WCAG 2.0-2.2 A/AA or axe best practices
//
// Run `npm run build` in tools/site first. Usage:
//
//   node audit.ts [--out <dir>] [--chrome <path>]
//
// --chrome (or CHROME_PATH) picks the browser; otherwise chrome-launcher
// finds an installed Chrome. Reports (Lighthouse HTML and JSON, summary.md)
// go to --out (default audit/report). Exits 1 when any page misses.

import { spawn } from "node:child_process";
import {
  appendFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import * as chromeLauncher from "chrome-launcher";
import lighthouse from "lighthouse";
import type { Flags, Result } from "lighthouse";
import desktopConfig from "lighthouse/core/config/desktop-config.js";
import { chromium } from "playwright-core";
import type { AxeResults, RunOptions } from "axe-core";

/** Minimum Lighthouse category scores (0-100) by category id, and optionally `cls`, the largest layout shift allowed. */
export type Limits = Readonly<Record<string, number>>;

/** The parts of a Lighthouse result the thresholds read. */
export interface LighthouseScores {
  /** Category scores (0-1, or null when Lighthouse could not score one) by category id. */
  categories: Readonly<Record<string, { score: number | null }>>;
  /** Audit results by audit id; only `cumulative-layout-shift` is read. */
  audits: Readonly<Record<string, { numericValue?: number }>>;
}

/** One audited page. */
export interface AuditPage {
  /** Short name for reports and file names. */
  name: string;
  /** The path requested. */
  path: string;
  /** The status the page must answer with. */
  status: number;
  /** Its thresholds. */
  limits: Limits;
}

/** A kept Lighthouse run: the result, its JSON and HTML reports, and the limits it misses. */
interface LighthouseRun {
  /** The Lighthouse result. */
  lhr: Result;
  /** The JSON report, then the HTML report. */
  report: [string, string];
  /** One line per missed limit. */
  misses: string[];
}

/** This directory (tools/site/audit). */
const HERE = dirname(fileURLToPath(import.meta.url));
/** The site package (tools/site). */
const SITE = join(HERE, "..");
/** This checkout's docs, served in place of the docs repository. */
const DOCS_DIR = join(SITE, "..", "..", "docs");

/** Docs key the local docs server fails, to render the docs-unavailable page. */
export const UNAVAILABLE_KEY = "audit-docs-unavailable";

/**
 * Minimum Lighthouse category scores (0-100) and the largest cumulative
 * layout shift for pages that answer 200. Performance allows for lab noise on
 * shared CI runners.
 */
export const PAGE: Limits = {
  performance: 90,
  accessibility: 100,
  "best-practices": 95,
  seo: 100,
  cls: 0.05,
};
/** The same for the 404 and docs-unavailable pages: accessibility only. */
export const ERROR_PAGE: Limits = { accessibility: 100 };

/**
 * Pages audited: a name, the path, the status it must answer with and its
 * thresholds.
 */
export const PAGES: readonly AuditPage[] = [
  { name: "home", path: "/", status: 200, limits: PAGE },
  { name: "download", path: "/download", status: 200, limits: PAGE },
  { name: "privacy", path: "/privacy", status: 200, limits: PAGE },
  { name: "security", path: "/security", status: 200, limits: PAGE },
  { name: "docs", path: "/docs/guide/getting-started", status: 200, limits: PAGE },
  { name: "docs-ci", path: "/docs/contributing/ci", status: 200, limits: PAGE },
  { name: "docs-unavailable", path: `/docs/${UNAVAILABLE_KEY}`, status: 503, limits: ERROR_PAGE },
  { name: "not-found", path: "/audit-no-such-page", status: 404, limits: ERROR_PAGE },
];

/** axe-core rule tags checked. */
export const AXE_TAGS = ["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa", "best-practice"];
/** Viewport widths (px) axe checks each page at. */
export const AXE_WIDTHS = [1350, 390];

/**
 * Lists the Lighthouse misses for one report.
 *
 * @param lhr - The Lighthouse result.
 * @param limits - Minimum category scores (0-100) and optionally `cls`, the
 *   largest layout shift allowed.
 * @returns One line per miss; empty when every limit holds.
 */
export function lighthouseMisses(lhr: LighthouseScores, limits: Limits): string[] {
  const misses: string[] = [];
  for (const [key, limit] of Object.entries(limits)) {
    if (key === "cls") {
      const cls = lhr.audits["cumulative-layout-shift"]?.numericValue;
      if (cls === undefined || cls > limit) misses.push(`CLS ${cls ?? "missing"} > ${limit}`);
      continue;
    }
    const score = lhr.categories[key]?.score;
    const pct = score === null || score === undefined ? null : Math.round(score * 100);
    if (pct === null || pct < limit) misses.push(`${key} ${pct ?? "missing"} < ${limit}`);
  }
  return misses;
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
 * Starts the local docs server: `/<ref>/<key>.md` answers docs/<key>.md for
 * any ref, UNAVAILABLE_KEY answers 503, anything else 404.
 *
 * @returns Its origin and a stop function.
 */
function serveDocs(): Promise<{ origin: string; close: () => void }> {
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? "/", "http://localhost").pathname;
    if (path.endsWith(`/${UNAVAILABLE_KEY}.md`)) {
      res.writeHead(503).end();
      return;
    }
    const file = docsFile(DOCS_DIR, path);
    if (
      file === null ||
      !existsSync(file) ||
      !realpathSync(file).startsWith(realpathSync(DOCS_DIR) + sep)
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

/**
 * Starts `wrangler dev` on the audit Worker and waits until it serves `/`.
 *
 * @param docsOrigin - The local docs server.
 * @param logFile - Where wrangler's output goes.
 * @returns The site origin and a stop function.
 * @throws {Error} When wrangler exits or does not serve within two minutes.
 */
async function serveSite(
  docsOrigin: string,
  logFile: string,
): Promise<{ origin: string; stop: () => void }> {
  const config = writeWranglerConfig();
  const port = await freePort();
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
  ];
  args.push("--var", `AUDIT_DOCS_ORIGIN:${docsOrigin}`);
  const child = spawn(wrangler, args, {
    cwd: HERE,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "true" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => appendFileSync(logFile, chunk));
  child.stderr.on("data", (chunk) => appendFileSync(logFile, chunk));
  let exited: number | null = null;
  child.on("exit", (code) => (exited = code ?? -1));
  const origin = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    if (exited !== null) throw new Error(`wrangler dev exited with ${exited}; see ${logFile}`);
    try {
      const res = await fetch(`${origin}/`);
      if (res.ok) return { origin, stop: () => child.kill("SIGTERM") };
    } catch {
      // Not listening yet.
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  child.kill("SIGTERM");
  throw new Error(`wrangler dev did not serve / within two minutes; see ${logFile}`);
}

/** The part of chrome-launcher this audit uses, so a test can stand in for it. */
export type ChromeLauncher = Pick<typeof chromeLauncher, "launch" | "killAll">;

/** A running Chrome and the profile directory it was given, which `kill` does not remove. */
export interface AuditChrome {
  /** The browser, with the debugging port Lighthouse connects to. */
  chrome: chromeLauncher.LaunchedChrome;
  /** Its `--user-data-dir`; remove it after `chrome.kill()`. */
  userDataDir: string;
}

/** How many lines of a failed start's `chrome-err.log` go to stderr. */
const CHROME_ERR_TAIL = 40;

/**
 * Whether a failed start is worth another try. chrome-launcher's own errors
 * (`ERR_LAUNCHER_*`: no browser, unsupported platform, unusable profile
 * directory) fail the same way every time. Anything else is the browser not
 * answering in time: a plain `Error` when it never reported its debugging
 * port, a socket error when it reported the port but did not accept a
 * connection; both are seen on busy CI runners. A browser that died at once
 * looks the same from here and costs one more wait before the audit fails.
 *
 * @param err - What `launch` rejected with.
 * @returns True for a start that may work the next time.
 */
export function isTransientStartFailure(err: unknown): boolean {
  if (!(err instanceof Error)) return false;
  const code: unknown = Reflect.get(err, "code");
  return !(typeof code === "string" && code.startsWith("ERR_LAUNCHER_"));
}

/**
 * Removes a profile directory Chrome may still be letting go of; a directory
 * that stays is left for the OS's temporary-file cleanup rather than failing
 * the audit.
 *
 * @param dir - The profile directory.
 */
function removeProfile(dir: string): void {
  try {
    rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 250 });
  } catch (err) {
    console.error(`could not remove ${dir}: ${err instanceof Error ? err.message : String(err)}`);
  }
}

/**
 * Keeps a failed start's `chrome-err.log`: a copy at `to` and its last
 * CHROME_ERR_TAIL lines on stderr. The log is a diagnostic aid, so a log that
 * is missing or cannot be read or copied is reported and does not replace the
 * start's own error.
 *
 * @param from - The log in the profile directory.
 * @param to - Where the copy goes.
 */
function keepChromeErrLog(from: string, to: string): void {
  if (!existsSync(from)) return;
  try {
    const text = readFileSync(from, "utf8");
    writeFileSync(to, text);
    const lines = text.trimEnd().split("\n");
    if (lines[0] === "" && lines.length === 1) return;
    console.error(`last ${Math.min(lines.length, CHROME_ERR_TAIL)} lines of chrome-err.log:`);
    for (const line of lines.slice(-CHROME_ERR_TAIL)) console.error(`  ${line}`);
  } catch (err) {
    console.error(`could not keep ${from}: ${err instanceof Error ? err.message : String(err)}`);
  }
}

/**
 * Starts Chrome for Lighthouse, once more when the first start never reaches
 * its debugging port, and keeps each failed start's `chrome-err.log` (tail on
 * stderr, copy in `out` as `chrome-err-<attempt>.log`), so the cause travels
 * with the uploaded reports. A browser whose start timed out is still
 * running; it is killed before the next start.
 *
 * @param launcher - chrome-launcher, or a stand-in.
 * @param chromePath - The browser.
 * @param chromeFlags - Flags for the browser.
 * @param out - The report directory the logs go to.
 * @param attempts - How many starts to try.
 * @returns The browser and its profile directory.
 * @throws The last attempt's error when none started, or the first error
 *   that another start would not fix.
 */
export async function launchChrome(
  launcher: ChromeLauncher,
  chromePath: string,
  chromeFlags: string[],
  out: string,
  attempts = 2,
): Promise<AuditChrome> {
  for (let attempt = 1; ; attempt++) {
    // Our own profile directory rather than chrome-launcher's temporary one, so
    // its chrome-err.log is still there to read after a failed start.
    const userDataDir = mkdtempSync(join(tmpdir(), "bookclerk-audit-chrome-"));
    try {
      const chrome = await launcher.launch({ chromePath, chromeFlags, userDataDir });
      return { chrome, userDataDir };
    } catch (err) {
      // chrome-launcher leaves a browser that never answered running and
      // gives no handle to it, only killAll (this audit starts one Chrome).
      for (const killErr of launcher.killAll())
        console.error(`could not stop Chrome: ${killErr.message}`);
      const message = err instanceof Error ? err.message : String(err);
      console.error(`Chrome did not start (attempt ${attempt} of ${attempts}): ${message}`);
      keepChromeErrLog(join(userDataDir, "chrome-err.log"), join(out, `chrome-err-${attempt}.log`));
      removeProfile(userDataDir);
      if (attempt >= attempts || !isTransientStartFailure(err)) throw err;
      console.error("trying again");
    }
  }
}

/**
 * Runs Lighthouse on one page, once more when only performance misses (lab
 * noise on shared runners), and keeps the better run.
 *
 * @param url - The page.
 * @param form - The form factor.
 * @param port - Chrome's debugging port.
 * @param limits - The page's thresholds.
 * @returns The kept run.
 */
async function runLighthouse(
  url: string,
  form: "mobile" | "desktop",
  port: number,
  limits: Limits,
): Promise<LighthouseRun> {
  const flags: Flags = {
    port,
    output: ["json", "html"],
    logLevel: "error",
    ignoreStatusCode: true,
    onlyCategories: ["performance", "accessibility", "best-practices", "seo"],
  };
  const config = form === "desktop" ? desktopConfig : undefined;
  let best: LighthouseRun | null = null;
  for (let attempt = 0; attempt < 2; attempt++) {
    const result = await lighthouse(url, flags, config);
    if (!result) throw new Error(`Lighthouse returned nothing for ${url}`);
    const [json, html] = result.report;
    if (typeof json !== "string" || typeof html !== "string")
      throw new Error(`Lighthouse returned no JSON and HTML reports for ${url}`);
    const run: LighthouseRun = {
      lhr: result.lhr,
      report: [json, html],
      misses: lighthouseMisses(result.lhr, limits),
    };
    if (best === null || run.misses.length < best.misses.length) best = run;
    if (!best.misses.every((m) => m.startsWith("performance "))) break;
    if (best.misses.length === 0) break;
  }
  if (best === null) throw new Error(`Lighthouse did not run for ${url}`);
  return best;
}

/**
 * Stops a Chrome from `launchChrome` and removes its profile directory.
 *
 * @param running - The browser and its profile directory.
 */
async function stopChrome(running: AuditChrome): Promise<void> {
  await running.chrome.kill();
  removeProfile(running.userDataDir);
}

/**
 * Runs axe-core on every page, light and dark, at each AXE_WIDTHS width.
 *
 * @param origin - The site origin.
 * @param chromePath - The browser.
 * @returns One line per violation; empty when clean.
 */
async function runAxe(origin: string, chromePath: string): Promise<string[]> {
  const axeSource = readFileSync(join(HERE, "node_modules", "axe-core", "axe.min.js"), "utf8");
  const browser = await chromium.launch({ executablePath: chromePath, args: ["--no-sandbox"] });
  const found: string[] = [];
  try {
    for (const colorScheme of ["light", "dark"] as const) {
      for (const width of AXE_WIDTHS) {
        for (const page of PAGES) {
          // The site's CSP forbids inline scripts; bypass it so axe can be injected.
          const context = await browser.newContext({
            colorScheme,
            viewport: { width, height: 900 },
            bypassCSP: true,
          });
          const tab = await context.newPage();
          await tab.goto(origin + page.path, { waitUntil: "load" });
          await tab.addScriptTag({ content: axeSource });
          const violations = await tab.evaluate(async (tags) => {
            // axe is the script just added.
            const { axe } = window as unknown as {
              axe: { run(context: Document, options: RunOptions): Promise<AxeResults> };
            };
            const result = await axe.run(document, { runOnly: tags });
            return result.violations.map(
              (v) =>
                `${v.id} (${v.impact}): ${v.nodes
                  .map((n) => n.target.join(" "))
                  .slice(0, 3)
                  .join(", ")}`,
            );
          }, AXE_TAGS);
          for (const v of violations) found.push(`${page.name} ${colorScheme} ${width}px: ${v}`);
          await context.close();
        }
      }
    }
  } finally {
    await browser.close();
  }
  return found;
}

/**
 * Finds the browser: CHROME_PATH, else an installed Chrome.
 *
 * @returns The executable.
 * @throws {Error} When there is none.
 */
function findChrome(): string {
  try {
    return chromeLauncher.getChromePath();
  } catch {
    throw new Error("no Chrome found: pass --chrome or set CHROME_PATH");
  }
}

/**
 * Runs the whole audit.
 *
 * @returns The exit code: 0 when every page passes, 1 otherwise.
 */
async function main(): Promise<number> {
  const { values } = parseArgs({
    options: { out: { type: "string", default: join(HERE, "report") }, chrome: { type: "string" } },
  });
  const out = resolve(values.out);
  mkdirSync(out, { recursive: true });
  const chromePath = values.chrome ?? findChrome();
  if (!existsSync(join(SITE, "dist", "index.html")))
    throw new Error("tools/site/dist is missing: run `npm run build` first");

  const docs = await serveDocs();
  let site: { stop: () => void } | null = null;
  let chrome: AuditChrome | null = null;
  const failures: string[] = [];
  const rows = [
    "| Page | Form | Status | Perf | A11y | BP | SEO | CLS | Result |",
    "| --- | --- | --- | --- | --- | --- | --- | --- | --- |",
  ];
  try {
    const served = await serveSite(docs.origin, join(out, "wrangler.log"));
    site = served;
    for (const page of PAGES) {
      const res = await fetch(served.origin + page.path);
      if (res.status !== page.status)
        failures.push(`${page.name}: ${page.path} answered ${res.status}, expected ${page.status}`);
    }

    // The proxy flags only matter where HTTPS_PROXY is set (not on CI runners).
    const flags = ["--headless=new", "--no-sandbox", "--disable-gpu"];
    const proxy = process.env["HTTPS_PROXY"];
    if (proxy) flags.push(`--proxy-server=${proxy}`, "--proxy-bypass-list=127.0.0.1;localhost");
    chrome = await launchChrome(chromeLauncher, chromePath, flags, out);
    for (const page of PAGES) {
      for (const form of ["mobile", "desktop"] as const) {
        const run = await runLighthouse(
          served.origin + page.path,
          form,
          chrome.chrome.port,
          page.limits,
        );
        writeFileSync(join(out, `${page.name}-${form}.json`), run.report[0]);
        writeFileSync(join(out, `${page.name}-${form}.html`), run.report[1]);
        const c = run.lhr.categories;
        const pct = (k: string): string => {
          const score = c[k]?.score;
          return score === null || score === undefined ? "–" : String(Math.round(score * 100));
        };
        const cls = run.lhr.audits["cumulative-layout-shift"]?.numericValue;
        rows.push(
          `| ${page.name} | ${form} | ${page.status} | ${pct("performance")} | ${pct("accessibility")} | ${pct("best-practices")} | ${pct("seo")} | ${cls === undefined ? "–" : cls.toFixed(3)} | ${run.misses.length ? "❌ " + run.misses.join("; ") : "✅"} |`,
        );
        for (const miss of run.misses) failures.push(`${page.name} (${form}): ${miss}`);
      }
    }
    await stopChrome(chrome);
    chrome = null;

    const axe = await runAxe(served.origin, chromePath);
    failures.push(...axe.map((v) => `axe: ${v}`));
  } finally {
    if (chrome !== null) await stopChrome(chrome);
    site?.stop();
    docs.close();
  }

  const summary = [
    "## Website audit",
    "",
    ...rows,
    "",
    failures.length
      ? `**${failures.length} problem(s):**\n\n${failures.map((f) => `- ${f}`).join("\n")}`
      : "Every page passed Lighthouse and axe-core.",
    "",
  ].join("\n");
  writeFileSync(join(out, "summary.md"), summary);
  const stepSummary = process.env["GITHUB_STEP_SUMMARY"];
  if (stepSummary) appendFileSync(stepSummary, summary);
  console.log(summary);
  return failures.length ? 1 : 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().then(
    (code) => process.exit(code),
    (err) => {
      console.error(err instanceof Error ? err.stack : err);
      process.exit(1);
    },
  );
}
