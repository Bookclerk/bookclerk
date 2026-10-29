// Submits the site's new and changed URLs to IndexNow, from the indexnow
// workflow (.github/workflows/indexnow.yml). Run with Node (no install
// needed, the module imports nothing outside this directory):
//
//   INDEXNOW_KEY=<key> SITE_ORIGIN=https://bookclerk.app \
//     node indexnow-run.ts <snapshot-in> <snapshot-out>
//
// It checks that <origin>/<key>.txt serves the key, reads every sitemap the
// live /sitemap.xml index names, fingerprints each URL (its `<lastmod>`, else
// a SHA-256 of the page as Markdown), plans the submission against the
// previous snapshot (indexnow.ts) and posts it. The new snapshot is written
// only after IndexNow accepted every request, so a failed run is retried in
// full. INDEXNOW_DRY_RUN=1 plans and prints without checking the key file,
// posting or writing, for a look before the key is deployed.
//
// An unset INDEXNOW_KEY is not an error: the run says so and exits, so a fork
// without a key never submits anything.

import { createHash } from "node:crypto";
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";

import {
  INDEXNOW_ENDPOINT,
  ORIGIN_RE,
  MAX_DOCUMENT_BYTES,
  parseKey,
  planSubmission,
  readSitemap,
  readSnapshot,
  submissionBodies,
  writeSnapshot,
} from "./indexnow.ts";
import type { IndexNowKey, SiteUrl, Snapshot } from "./indexnow.ts";
import { parseIsoDate } from "./last-updated.ts";

/** How long one request may take, in milliseconds. */
const REQUEST_TIMEOUT_MS = 30_000;

/** Pages fetched at once to fingerprint URLs that have no `<lastmod>`. */
const FETCH_CONCURRENCY = 4;

/** URLs the job summary lists before it says how many more there are. */
const SUMMARY_URLS = 50;

/** A run that stops without submitting, with the reason to print. */
class Stop extends Error {}

/** What a run reads and writes outside this module, so tests can stand in for it. */
export interface RunDeps {
  /** The environment: INDEXNOW_KEY, SITE_ORIGIN, INDEXNOW_DRY_RUN, GITHUB_STEP_SUMMARY. */
  env: Readonly<Record<string, string | undefined>>;
  /** Fetches a URL. */
  fetch: typeof fetch;
  /** The run's UTC day, `YYYY-MM-DD`. */
  today: string;
  /** Reads a file, or returns null when it does not exist. */
  readFile: (path: string) => string | null;
  /** Writes a file. */
  writeFile: (path: string, text: string) => void;
  /** Appends to a file. */
  appendFile: (path: string, text: string) => void;
  /** Prints a line to stdout. */
  log: (line: string) => void;
}

/**
 * Fetches a URL of the site and returns its body.
 *
 * Redirects are refused: every URL the sitemaps list is served as itself.
 *
 * @param deps - The fetcher.
 * @param url - The URL.
 * @param accept - The `Accept` header to send.
 * @returns The body as text.
 * @throws {Stop} When the request fails (network, timeout or a redirect),
 *   the response is not a 200, or it is larger than MAX_DOCUMENT_BYTES.
 */
async function fetchText(deps: RunDeps, url: string, accept: string): Promise<string> {
  let response: Response;
  try {
    response = await deps.fetch(url, {
      headers: { Accept: accept, "User-Agent": "bookclerk-indexnow (+https://www.indexnow.org)" },
      redirect: "error",
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
  } catch (err) {
    throw new Stop(`${url}: ${err instanceof Error ? err.message : String(err)}`);
  }
  if (response.status !== 200) throw new Stop(`${url} answered ${response.status}`);
  const length = Number(response.headers.get("Content-Length") ?? "0");
  if (length > MAX_DOCUMENT_BYTES) throw new Stop(`${url} is larger than the reader accepts`);
  const text = await response.text();
  if (text.length > MAX_DOCUMENT_BYTES) throw new Stop(`${url} is larger than the reader accepts`);
  return text;
}

/**
 * Reads every URL the live sitemaps list, with its fingerprint.
 *
 * A page without a `<lastmod>` that cannot be fetched keeps the
 * fingerprint the previous snapshot gives it, so it reads as unchanged
 * rather than removed, with a warning; one that is not in the snapshot
 * either is left out until a later run can fetch it.
 *
 * @param deps - The fetcher and logger.
 * @param origin - The site's origin.
 * @param previous - The previous snapshot, for pages that cannot be fetched.
 * @returns Fingerprint by URL.
 * @throws {Stop} When /sitemap.xml is not an index (the Worker's fallback
 *   while the docs dates cannot be read) or a section cannot be read; a
 *   partial list would read as removed pages.
 */
async function currentUrls(
  deps: RunDeps,
  origin: string,
  previous: Snapshot | null,
): Promise<Map<SiteUrl, string>> {
  const index = readSitemap(
    await fetchText(deps, `${origin}/sitemap.xml`, "application/xml"),
    origin,
  );
  if (index?.kind !== "index")
    throw new Stop("/sitemap.xml is not a sitemap index (the docs dates may be unreadable)");
  const undated: SiteUrl[] = [];
  const current = new Map<SiteUrl, string>();
  for (const { url: section } of index.entries) {
    if (!/^\/sitemap-[a-z0-9-]+\.xml$/.test(new URL(section).pathname))
      throw new Stop(`the sitemap index names ${section}, not a section sitemap`);
    const sitemap = readSitemap(await fetchText(deps, section, "application/xml"), origin);
    if (sitemap?.kind !== "urlset") throw new Stop(`${section} is not a sitemap`);
    for (const { url, lastmod } of sitemap.entries) {
      if (lastmod === null) undated.push(url);
      else current.set(url, `lastmod:${lastmod}`);
    }
  }
  for (let at = 0; at < undated.length; at += FETCH_CONCURRENCY) {
    const batch = undated.slice(at, at + FETCH_CONCURRENCY);
    const bodies = await Promise.all(
      batch.map((url) =>
        fetchText(deps, url, "text/markdown").catch((err: unknown) => {
          if (!(err instanceof Stop)) throw err;
          deps.log(`::warning::${err.message}; it keeps its previous fingerprint`);
          return null;
        }),
      ),
    );
    batch.forEach((url, i) => {
      const body = bodies[i] ?? null;
      const kept = previous?.get(url)?.fp;
      if (body !== null)
        current.set(url, `sha256:${createHash("sha256").update(body).digest("hex")}`);
      else if (kept !== undefined) current.set(url, kept);
    });
  }
  return current;
}

/**
 * Checks that the site serves the key file IndexNow will fetch.
 *
 * @param deps - The fetcher.
 * @param origin - The site's origin.
 * @param key - The key.
 * @throws {Stop} When `<origin>/<key>.txt` is missing or holds something else.
 */
async function checkKeyFile(deps: RunDeps, origin: string, key: IndexNowKey): Promise<void> {
  const body = await fetchText(deps, `${origin}/${key}.txt`, "text/plain");
  if (body.trim() !== key)
    throw new Stop(`${origin}/${key}.txt does not hold the key; deploy the site with it first`);
}

/**
 * Posts the submission to IndexNow.
 *
 * @param deps - The fetcher.
 * @param bodies - The request bodies (submissionBodies).
 * @throws {Error} When IndexNow answers anything but 200 or 202.
 */
async function post(deps: RunDeps, bodies: readonly string[]): Promise<void> {
  for (const body of bodies) {
    const response = await deps.fetch(INDEXNOW_ENDPOINT, {
      method: "POST",
      headers: { "Content-Type": "application/json; charset=utf-8" },
      body,
      redirect: "error",
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    // 202 means the key is still being checked; the URLs were received.
    if (response.status !== 200 && response.status !== 202) {
      const reason = (await response.text()).slice(0, 500);
      throw new Error(`IndexNow answered ${response.status}: ${reason}`);
    }
  }
}

/**
 * Appends Markdown to the job summary, when there is one.
 *
 * @param deps - The environment and file writer.
 * @param markdown - The text.
 */
function summary(deps: RunDeps, markdown: string): void {
  const file = deps.env["GITHUB_STEP_SUMMARY"];
  if (file) deps.appendFile(file, markdown);
}

/**
 * Runs one submission.
 *
 * @param argv - `<snapshot-in> <snapshot-out>`.
 * @param deps - The environment, fetcher, clock and files.
 * @returns The exit status: 0 when submitted, skipped or nothing changed,
 *   1 when the configuration is wrong or IndexNow refused, 2 for bad usage.
 */
export async function run(argv: readonly string[], deps: RunDeps): Promise<number> {
  const [snapshotIn, snapshotOut] = argv;
  if (snapshotIn === undefined || snapshotOut === undefined) {
    deps.log("usage: node indexnow-run.ts <snapshot-in> <snapshot-out>");
    return 2;
  }
  const keyText = (deps.env["INDEXNOW_KEY"] ?? "").trim();
  if (keyText === "") {
    deps.log("INDEXNOW_KEY is not set; nothing submitted.");
    return 0;
  }
  const key = parseKey(keyText);
  if (key === null) {
    deps.log("::error::INDEXNOW_KEY must be 8-128 letters, digits or '-'");
    return 1;
  }
  const origin = (deps.env["SITE_ORIGIN"] ?? "").trim() || "https://bookclerk.app";
  if (!ORIGIN_RE.test(origin)) {
    deps.log(`::error::SITE_ORIGIN must be https://<lowercase hostname>, got ${origin}`);
    return 1;
  }
  const today = parseIsoDate(deps.today);
  if (today === null) {
    deps.log(`::error::not a date: ${deps.today}`);
    return 1;
  }
  const dryRun = deps.env["INDEXNOW_DRY_RUN"] === "1";

  try {
    if (!dryRun) await checkKeyFile(deps, origin, key);
    const stored = deps.readFile(snapshotIn);
    const previous = stored === null ? null : readSnapshot(stored, origin);
    const current = await currentUrls(deps, origin, previous);
    const plan = planSubmission(previous, current, today);
    const what = previous === null ? "no previous snapshot, so every URL" : "new or changed URLs";
    deps.log(`${current.size} URLs listed; submitting ${plan.submit.length} (${what}).`);
    if (dryRun) {
      for (const url of plan.submit) deps.log(url);
    } else {
      await post(deps, submissionBodies(origin, key, plan.submit));
      deps.writeFile(snapshotOut, writeSnapshot(plan.next));
    }
    const shown = plan.submit.slice(0, SUMMARY_URLS).map((url) => `- ${url}\n`);
    const more = plan.submit.length - shown.length;
    const done = dryRun ? "to submit (dry run)" : "submitted";
    summary(
      deps,
      `### IndexNow\n\n${current.size} URLs listed; ${plan.submit.length} ${done} (${what}).\n\n` +
        `${shown.join("")}${more > 0 ? `- ...and ${more} more\n` : ""}`,
    );
    return 0;
  } catch (err) {
    if (err instanceof Stop) {
      // Nothing was submitted and the old snapshot stays; the next run retries.
      deps.log(`::warning::IndexNow skipped: ${err.message}`);
      return 0;
    }
    deps.log(`::error::${err instanceof Error ? err.message : String(err)}`);
    return 1;
  }
}

/* v8 ignore start -- the command line itself; tests call run() */
// import.meta.main needs Node 24.2; the site supports Node 22.18.
if (process.argv[1] === import.meta.filename) {
  process.exitCode = await run(process.argv.slice(2), {
    env: process.env,
    fetch,
    today: new Date().toISOString().slice(0, 10),
    readFile: (path) => (existsSync(path) ? readFileSync(path, "utf8") : null),
    writeFile: (path, text) => writeFileSync(path, text),
    appendFile: (path, text) => appendFileSync(path, text),
    log: (line) => console.log(line),
  });
}
/* v8 ignore stop */
