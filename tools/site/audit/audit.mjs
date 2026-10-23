// Lighthouse and axe-core audit of the built bookclerk.app website, run by CI
// for pull requests that change the site (scripts/ci_plan, check
// `site_audit`). See docs/ci.md#website-checks.
//
// It serves tools/site/dist through the real Worker under `wrangler dev`
// (audit/entry.mjs, with docs read from this checkout's docs/ through a local
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
//   node audit.mjs [--out <dir>] [--chrome <path>]
//
// --chrome (or CHROME_PATH) picks the browser; otherwise chrome-launcher
// finds an installed Chrome. Reports (Lighthouse HTML and JSON, summary.md)
// go to --out (default audit/report). Exits 1 when any page misses.

import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { createServer as createNetServer } from "node:net";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import * as chromeLauncher from "chrome-launcher";
import lighthouse from "lighthouse";
import desktopConfig from "lighthouse/core/config/desktop-config.js";
import { chromium } from "playwright-core";

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
export const PAGE = { performance: 90, accessibility: 100, "best-practices": 95, seo: 100, cls: 0.05 };
/** The same for the 404 and docs-unavailable pages: accessibility only. */
export const ERROR_PAGE = { accessibility: 100 };

/**
 * Pages audited: a name, the path, the status it must answer with and its
 * thresholds.
 *
 * @type {{name: string, path: string, status: number, limits: Record<string, number>}[]}
 */
export const PAGES = [
  { name: "home", path: "/", status: 200, limits: PAGE },
  { name: "download", path: "/download", status: 200, limits: PAGE },
  { name: "docs", path: "/docs/getting-started", status: 200, limits: PAGE },
  { name: "docs-ci", path: "/docs/ci", status: 200, limits: PAGE },
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
 * @param {{categories: Record<string, {score: number | null}>, audits: Record<string, {numericValue?: number}>}} lhr
 *   The Lighthouse result.
 * @param {Record<string, number>} limits Minimum category scores (0-100) and
 *   optionally `cls`, the largest layout shift allowed.
 * @returns {string[]} One line per miss; empty when every limit holds.
 */
export function lighthouseMisses(lhr, limits) {
  const misses = [];
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
 * @param {string} docsDir The docs directory.
 * @param {string} pathname The request path, `/<ref>/<key>.md`.
 * @returns {string | null} The file, or null when the path is malformed or
 *   leaves docsDir.
 */
export function docsFile(docsDir, pathname) {
  const match = /^\/[^/]+\/(.+\.md)$/.exec(decodeURIComponent(pathname));
  if (match === null) return null;
  const root = resolve(docsDir);
  const file = resolve(root, match[1]);
  if (relative(root, file).startsWith("..") || !file.startsWith(root + sep)) return null;
  return file;
}

/**
 * Starts the local docs server: `/<ref>/<key>.md` answers docs/<key>.md for
 * any ref, UNAVAILABLE_KEY answers 503, anything else 404.
 *
 * @returns {Promise<{origin: string, close: () => void}>} Its origin and a stop function.
 */
function serveDocs() {
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? "/", "http://localhost").pathname;
    if (path.endsWith(`/${UNAVAILABLE_KEY}.md`)) {
      res.writeHead(503).end();
      return;
    }
    const file = docsFile(DOCS_DIR, path);
    if (file === null || !existsSync(file) || !realpathSync(file).startsWith(realpathSync(DOCS_DIR) + sep)) {
      res.writeHead(404).end();
      return;
    }
    res.writeHead(200, { "Content-Type": "text/markdown; charset=utf-8" }).end(readFileSync(file));
  });
  return new Promise((done) => {
    server.listen(0, "127.0.0.1", () => {
      const { port } = /** @type {import("node:net").AddressInfo} */ (server.address());
      done({ origin: `http://127.0.0.1:${port}`, close: () => server.close() });
    });
  });
}

/**
 * Finds a free TCP port on 127.0.0.1.
 *
 * @returns {Promise<number>} The port.
 */
function freePort() {
  return new Promise((done, fail) => {
    const probe = createNetServer();
    probe.on("error", fail);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = /** @type {import("node:net").AddressInfo} */ (probe.address());
      probe.close(() => done(port));
    });
  });
}

/**
 * Writes the audit Worker's wrangler config: ../wrangler.toml with the audit
 * entry point, so the assets and routing cannot drift from the deployed site.
 *
 * @returns {string} The config file.
 * @throws {Error} When ../wrangler.toml no longer has the lines replaced.
 */
function writeWranglerConfig() {
  const base = readFileSync(join(SITE, "wrangler.toml"), "utf8");
  const replacements = [
    [/^name = .*$/m, 'name = "bookclerk-site-audit"'],
    [/^main = .*$/m, 'main = "entry.mjs"'],
    [/^directory = .*$/m, 'directory = "../dist"'],
  ];
  let config = base;
  for (const [pattern, line] of replacements) {
    if (!pattern.test(config)) throw new Error(`tools/site/wrangler.toml has no line matching ${pattern}`);
    config = config.replace(pattern, line);
  }
  const file = join(HERE, "wrangler.generated.toml");
  writeFileSync(file, config);
  return file;
}

/**
 * Starts `wrangler dev` on the audit Worker and waits until it serves `/`.
 *
 * @param {string} docsOrigin The local docs server.
 * @param {string} logFile Where wrangler's output goes.
 * @returns {Promise<{origin: string, stop: () => void}>} The site origin and a stop function.
 * @throws {Error} When wrangler exits or does not serve within two minutes.
 */
async function serveSite(docsOrigin, logFile) {
  const config = writeWranglerConfig();
  const port = await freePort();
  const inspector = await freePort();
  const wrangler = join(SITE, "node_modules", ".bin", "wrangler");
  const args = ["dev", "--config", config, "--ip", "127.0.0.1", "--port", String(port), "--inspector-port", String(inspector)];
  args.push("--var", `AUDIT_DOCS_ORIGIN:${docsOrigin}`);
  const child = spawn(wrangler, args, {
    cwd: HERE,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "true" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => appendFileSync(logFile, chunk));
  child.stderr.on("data", (chunk) => appendFileSync(logFile, chunk));
  let exited = null;
  child.on("exit", (code) => (exited = code));
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

/**
 * Runs Lighthouse on one page, once more when only performance misses (lab
 * noise on shared runners), and keeps the better run.
 *
 * @param {string} url The page.
 * @param {"mobile" | "desktop"} form The form factor.
 * @param {number} port Chrome's debugging port.
 * @param {Record<string, number>} limits The page's thresholds.
 * @returns {Promise<{lhr: any, report: string[], misses: string[]}>} The kept run.
 */
async function runLighthouse(url, form, port, limits) {
  const flags = {
    port,
    output: ["json", "html"],
    logLevel: "error",
    ignoreStatusCode: true,
    onlyCategories: ["performance", "accessibility", "best-practices", "seo"],
  };
  const config = form === "desktop" ? desktopConfig : undefined;
  let best = null;
  for (let attempt = 0; attempt < 2; attempt++) {
    const result = await lighthouse(url, flags, config);
    if (!result) throw new Error(`Lighthouse returned nothing for ${url}`);
    const run = { lhr: result.lhr, report: /** @type {string[]} */ (result.report), misses: lighthouseMisses(result.lhr, limits) };
    if (best === null || run.misses.length < best.misses.length) best = run;
    if (!best.misses.every((m) => m.startsWith("performance "))) break;
    if (best.misses.length === 0) break;
  }
  return /** @type {{lhr: any, report: string[], misses: string[]}} */ (best);
}

/**
 * Runs axe-core on every page, light and dark, at each AXE_WIDTHS width.
 *
 * @param {string} origin The site origin.
 * @param {string} chromePath The browser.
 * @returns {Promise<string[]>} One line per violation; empty when clean.
 */
async function runAxe(origin, chromePath) {
  const axeSource = readFileSync(join(HERE, "node_modules", "axe-core", "axe.min.js"), "utf8");
  const browser = await chromium.launch({ executablePath: chromePath, args: ["--no-sandbox"] });
  const found = [];
  try {
    for (const colorScheme of /** @type {const} */ (["light", "dark"])) {
      for (const width of AXE_WIDTHS) {
        for (const page of PAGES) {
          // The site's CSP forbids inline scripts; bypass it so axe can be injected.
          const context = await browser.newContext({ colorScheme, viewport: { width, height: 900 }, bypassCSP: true });
          const tab = await context.newPage();
          await tab.goto(origin + page.path, { waitUntil: "load" });
          await tab.addScriptTag({ content: axeSource });
          const violations = await tab.evaluate(async (tags) => {
            // @ts-ignore axe is the script just added.
            const result = await window.axe.run(document, { runOnly: tags });
            return result.violations.map((v) => `${v.id} (${v.impact}): ${v.nodes.map((n) => n.target.join(" ")).slice(0, 3).join(", ")}`);
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
 * @returns {string} The executable.
 * @throws {Error} When there is none.
 */
function findChrome() {
  try {
    return chromeLauncher.getChromePath();
  } catch {
    throw new Error("no Chrome found: pass --chrome or set CHROME_PATH");
  }
}

/**
 * Runs the whole audit.
 *
 * @returns {Promise<number>} The exit code: 0 when every page passes, 1 otherwise.
 */
async function main() {
  const { values } = parseArgs({
    options: { out: { type: "string", default: join(HERE, "report") }, chrome: { type: "string" } },
  });
  const out = resolve(/** @type {string} */ (values.out));
  mkdirSync(out, { recursive: true });
  const chromePath = values.chrome ?? findChrome();
  if (!existsSync(join(SITE, "dist", "index.html"))) throw new Error("tools/site/dist is missing: run `npm run build` first");

  const docs = await serveDocs();
  /** @type {{stop: () => void} | null} */
  let site = null;
  /** @type {import("chrome-launcher").LaunchedChrome | null} */
  let chrome = null;
  const failures = [];
  const rows = ["| Page | Form | Status | Perf | A11y | BP | SEO | CLS | Result |", "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"];
  try {
    const served = await serveSite(docs.origin, join(out, "wrangler.log"));
    site = served;
    for (const page of PAGES) {
      const res = await fetch(served.origin + page.path);
      if (res.status !== page.status) failures.push(`${page.name}: ${page.path} answered ${res.status}, expected ${page.status}`);
    }

    // The proxy flags only matter where HTTPS_PROXY is set (not on CI runners).
    const flags = ["--headless=new", "--no-sandbox", "--disable-gpu"];
    if (process.env.HTTPS_PROXY) flags.push(`--proxy-server=${process.env.HTTPS_PROXY}`, "--proxy-bypass-list=127.0.0.1;localhost");
    chrome = await chromeLauncher.launch({ chromePath, chromeFlags: flags });
    for (const page of PAGES) {
      for (const form of /** @type {const} */ (["mobile", "desktop"])) {
        const run = await runLighthouse(served.origin + page.path, form, chrome.port, page.limits);
        writeFileSync(join(out, `${page.name}-${form}.json`), run.report[0]);
        writeFileSync(join(out, `${page.name}-${form}.html`), run.report[1]);
        const c = run.lhr.categories;
        const pct = (/** @type {string} */ k) => (c[k]?.score == null ? "–" : String(Math.round(c[k].score * 100)));
        const cls = run.lhr.audits["cumulative-layout-shift"]?.numericValue;
        rows.push(
          `| ${page.name} | ${form} | ${page.status} | ${pct("performance")} | ${pct("accessibility")} | ${pct("best-practices")} | ${pct("seo")} | ${cls === undefined ? "–" : cls.toFixed(3)} | ${run.misses.length ? "❌ " + run.misses.join("; ") : "✅"} |`,
        );
        for (const miss of run.misses) failures.push(`${page.name} (${form}): ${miss}`);
      }
    }
    await chrome.kill();
    chrome = null;

    const axe = await runAxe(served.origin, chromePath);
    failures.push(...axe.map((v) => `axe: ${v}`));
  } finally {
    if (chrome !== null) await chrome.kill();
    site?.stop();
    docs.close();
  }

  const summary = [
    "## Website audit",
    "",
    ...rows,
    "",
    failures.length ? `**${failures.length} problem(s):**\n\n${failures.map((f) => `- ${f}`).join("\n")}` : "Every page passed Lighthouse and axe-core.",
    "",
  ].join("\n");
  writeFileSync(join(out, "summary.md"), summary);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, summary);
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
