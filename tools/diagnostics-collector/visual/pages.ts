// The diagnostics collector pages the web UI's visual checks
// (ui/visual/workers.spec.ts) open on the local server serve.ts runs. Plain
// data with no imports, so the web UI's visual suite can read it too.

/** One collector page, rendered from fixed sample data. */
export interface CollectorPage {
  /** Short name for test titles and screenshot file names. */
  name: string;
  /** The path serve.ts answers it on. */
  path: string;
}

/**
 * Every page template the collector serves: the public sign-in and report
 * lookup pages, and the member dashboard.
 */
export const VISUAL_PAGES: readonly CollectorPage[] = [
  { name: "sign-in", path: "/" },
  { name: "message", path: "/message" },
  { name: "lookup-form", path: "/lookup" },
  { name: "lookup-result", path: "/lookup/result" },
  { name: "lookup-deleted", path: "/lookup/deleted" },
  { name: "lookup-message", path: "/lookup/message" },
  { name: "overview", path: "/dash" },
  { name: "reports", path: "/reports" },
  { name: "report", path: "/report" },
  { name: "issues", path: "/issues" },
  { name: "issue", path: "/issue" },
  { name: "demo", path: "/demo" },
];

/**
 * One building block of the collector's pages, screenshotted on its own: the
 * first element matching `selector` on the page named `page`.
 */
export interface CollectorComponent {
  /** Short name for test titles and screenshot file names. */
  name: string;
  /** The VISUAL_PAGES entry it is taken from. */
  page: string;
  /** CSS selector for the element; the first match is used. */
  selector: string;
}

/**
 * The collector's building blocks, each taken from a page that shows it, so a
 * change to one shows up as its own small image beside the full pages.
 */
export const VISUAL_COMPONENTS: readonly CollectorComponent[] = [
  { name: "sign-in-card", page: "sign-in", selector: "div.card.signin" },
  { name: "lookup-form", page: "lookup-form", selector: "form.lookup-form" },
  { name: "header", page: "overview", selector: "header.band" },
  { name: "toolbar", page: "overview", selector: "div.toolbar" },
  { name: "stat-card", page: "overview", selector: "section.stat" },
  { name: "bar-chart", page: "overview", selector: "section.panel:has(svg.chart)" },
  { name: "meter-table", page: "overview", selector: "section.panel:has(table svg.meter)" },
  { name: "signature-table", page: "overview", selector: "section.panel:has(div.tbl.stack)" },
  { name: "filters", page: "reports", selector: "form.filters" },
  { name: "bulk-actions", page: "reports", selector: "form.bulk" },
  { name: "report-meta", page: "report", selector: "dl.meta" },
  { name: "event-log", page: "report", selector: "div.log" },
  { name: "delete-form", page: "lookup-result", selector: "form.delete-form" },
  { name: "issue-table", page: "issues", selector: "section.panel:has(div.tbl.stack)" },
  { name: "issue-links", page: "issue", selector: "div.links" },
  { name: "banner", page: "issue", selector: "div.banner" },
  { name: "ledger", page: "issue", selector: "section.panel:has(td.actions)" },
];
