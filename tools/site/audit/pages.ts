// The website pages the audit (audit.ts) and the visual checks
// (ui/visual/workers.spec.ts) open on the local server serve.ts runs. Plain
// data with no imports, so the web UI's visual suite can read it too.

/** Docs key the local docs server fails, to render the docs-unavailable page. */
export const UNAVAILABLE_KEY = "audit-docs-unavailable";

/**
 * Docs key prefix the local docs server answers from audit/visual-docs/
 * instead of docs/: fixed pages that lay out every kind of docs block, so a
 * screenshot changes with the docs layout and not with what the docs say.
 */
export const FIXTURE_PREFIX = "audit-fixture/";

/** One page of the website, opened on the local server. */
export interface SitePage {
  /** Short name for reports, test titles and screenshot file names. */
  name: string;
  /** The path requested. */
  path: string;
  /** The status the page must answer with. */
  status: number;
}

/** Pages the audit runs Lighthouse and axe-core on. */
export const AUDIT_PAGES: readonly SitePage[] = [
  { name: "home", path: "/", status: 200 },
  { name: "download", path: "/download", status: 200 },
  { name: "privacy", path: "/privacy", status: 200 },
  { name: "security", path: "/security", status: 200 },
  { name: "docs", path: "/docs/guide/getting-started", status: 200 },
  { name: "docs-ci", path: "/docs/contributing/ci", status: 200 },
  { name: "docs-unavailable", path: `/docs/${UNAVAILABLE_KEY}`, status: 503 },
  { name: "not-found", path: "/audit-no-such-page", status: 404 },
];

/**
 * Pages the visual checks screenshot and check with axe-core: the audit's,
 * with the docs fixture in place of real docs pages. Real docs pages, and the
 * sitemap page that lists them, change with every edit to docs/ far more
 * often than their layout does.
 */
export const VISUAL_PAGES: readonly SitePage[] = [
  ...AUDIT_PAGES.filter((p) => p.name !== "docs" && p.name !== "docs-ci"),
  { name: "docs-layout", path: `/docs/${FIXTURE_PREFIX}layout`, status: 200 },
];
