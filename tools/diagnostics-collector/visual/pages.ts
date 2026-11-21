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
