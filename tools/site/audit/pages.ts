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

/**
 * One building block of the website, screenshotted on its own: the first
 * element matching `selector` on the VISUAL_PAGES entry named `page`.
 */
export interface SiteComponent {
  /** Short name for test titles and screenshot file names. */
  name: string;
  /** The VISUAL_PAGES entry it is taken from. */
  page: string;
  /** CSS selector for the element; the first match is used. */
  selector: string;
}

/**
 * The website's building blocks, each taken from a page that shows it, so a
 * change to one shows up as its own small image beside the full pages.
 */
export const VISUAL_COMPONENTS: readonly SiteComponent[] = [
  { name: "header", page: "download", selector: "header.site-header" },
  { name: "ribbon", page: "download", selector: "aside.ribbon" },
  { name: "footer", page: "download", selector: "footer.site-footer" },
  { name: "hero", page: "home", selector: "section.hero" },
  { name: "buttons", page: "home", selector: "div.btn-row" },
  { name: "feature", page: "home", selector: "div.feature" },
  { name: "steps", page: "home", selector: "ol.cards3" },
  { name: "platform-card", page: "download", selector: "div.platform" },
  { name: "status-card", page: "download", selector: "div.status-card" },
  { name: "faq", page: "download", selector: "section.faq" },
  { name: "docs-header", page: "docs-layout", selector: "header.docs-header" },
  { name: "breadcrumbs", page: "docs-layout", selector: "nav.crumbs" },
  { name: "callout-note", page: "docs-layout", selector: "div.callout-note" },
  { name: "callout-warning", page: "docs-layout", selector: "div.callout-warning" },
  { name: "code-block", page: "docs-layout", selector: "div.code" },
  { name: "table", page: "docs-layout", selector: "div.table-scroll" },
  { name: "diagram", page: "docs-layout", selector: "figure.diagram" },
];
