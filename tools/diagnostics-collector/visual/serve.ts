// Serves every collector page template rendered from fixed sample data, with
// the dashboard's static files, for the web UI's visual and accessibility
// checks (ui/visual/workers.spec.ts). The templates are pure functions
// (dashboard/pages.ts), so no database, GitHub App or session is needed, and
// the fixed data and clock make every render the same.
//
//   node visual/serve.ts --port <port>
//
// Runs until SIGTERM or SIGINT. Never deployed.

import { existsSync, readFileSync, realpathSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import type { FixStatus, IssueListRow, IssueRow, ReportListRow } from "../dashboard/db.ts";
import { SECURITY_HEADERS, html, type SafeHtml } from "../dashboard/html.ts";
import {
  demoPage,
  issuePage,
  issuesPage,
  lookupDeletedPage,
  lookupFormPage,
  lookupMessagePage,
  lookupResultPage,
  messagePage,
  overviewPage,
  reportPage,
  reportsPage,
  signInPage,
} from "../dashboard/pages.ts";
import type { Member } from "../dashboard/session.ts";
import { VISUAL_PAGES } from "./pages.ts";

/** The collector package (tools/diagnostics-collector). */
const COLLECTOR = join(dirname(fileURLToPath(import.meta.url)), "..");
/** The dashboard's static files, served under `/static/`. */
const STATIC_DIR = join(COLLECTOR, "public", "static");

/** One day in milliseconds. */
const DAY_MS = 86_400_000;
/** The fixed clock every page is rendered at: 2026-01-15 12:00 UTC. */
const NOW = Date.UTC(2026, 0, 15, 12);
/** Today's day index (Unix ms / one day). */
const TODAY = Math.floor(NOW / DAY_MS);
/** The repository pages link to. */
const CFG = { owner: "example-org", repo: "bookclerk" };
/** The signed-in member: an owner, so the owner-only parts show too. */
const MEMBER: Member = { userId: 1, login: "octo-owner", role: "admin", expiresAt: NOW + DAY_MS };

/** Sample report IDs. */
const IDS = [
  "3f2a9c4e-7b1d-4e8a-9c0f-1a2b3c4d5e6f",
  "8b7c6d5e-4f3a-4b2c-9d1e-0f9e8d7c6b5a",
  "c1d2e3f4-a5b6-4c7d-8e9f-a0b1c2d3e4f5",
  "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
];

/**
 * A sample indexed report.
 *
 * @param i - Which sample, 0-3; picks the ID, time, version and links.
 * @returns The row.
 */
function report(i: number): ReportListRow {
  const tracked = i % 2 === 0;
  return {
    report_id: IDS[i % IDS.length] ?? "",
    object_key: `diagnostics/0.1.${i}/${IDS[i % IDS.length] ?? ""}.json`,
    received_at: NOW - (i + 1) * 3_600_000,
    version: `0.1.${i}`,
    os: i === 1 ? "macos" : "linux",
    arch: i === 1 ? "aarch64" : "x86_64",
    distro: i === 1 ? "" : "debian 13",
    trigger: ["error_burst", "warn_burst", "error_burst", "manual"][i] ?? "manual",
    events: 120 + i * 7,
    errors: 2 - (i % 2),
    warnings: i,
    signature: "a1b2c3d4e5f60718",
    signature_label: "bookclerk_library::scan · account token expired · /api/library/scan",
    route: "/api/library/scan",
    peak_latency_ms: 840 + i * 10,
    deployment: i === 3 ? "demo" : "",
    triage_state: i === 1 ? "seen" : "",
    triaged_by: i === 1 ? "octo-owner" : null,
    muted: 0,
    issue_number: tracked ? 412 : null,
    issue_state: tracked ? "open" : null,
    issue_source: tracked ? "filed" : null,
    issue_count: tracked ? 1 : 0,
    deleted_at: null,
    rustc: "1.100.0 stable",
    build_commit: "0123456789abcdef0123456789abcdef01234567",
  };
}

/** A sample diagnostics issue. */
const ISSUE: IssueRow = {
  number: 412,
  title: "Library scan fails when an account token has expired",
  state: "open",
  kind: "triage",
  created_at: NOW - 6 * DAY_MS,
  updated_at: NOW - DAY_MS,
  closed_at: null,
  prs_synced_at: NOW - 3_600_000,
  fixed_in_label: null,
  regression_of: null,
  summary_sent: null,
  summary_sent_at: NOW - 2 * DAY_MS,
  summary_handed_at: null,
  summary_lock_until: null,
};

/** The sample issue's merged fix. */
const FIX: FixStatus = {
  fixedIn: "0.1.3",
  source: "release",
  prNumber: 415,
  fixAt: NOW - 2 * DAY_MS,
  after: 1,
};

/** A merged pull request for the sample issue. */
const PR = {
  issue_number: 412,
  pr_number: 415,
  title: "Refresh expired account tokens before a scan",
  state: "merged",
  merged_at: NOW - 2 * DAY_MS,
  merge_sha: "0123456789abcdef0123456789abcdef01234567",
};

/** Reports per day over the last two weeks, for the charts. */
const DAYS = new Map(Array.from({ length: 14 }, (_, i) => [TODAY - i, (i * 5) % 7] as const));

/** The sample issue as the issues board lists it. */
const ISSUE_ROW: IssueListRow = {
  ...ISSUE,
  reports: 9,
  first_seen: NOW - 6 * DAY_MS,
  last_seen: NOW - 3_600_000,
  first_version: "0.1.0",
  last_version: "0.1.3",
  days: DAYS,
  prs: [PR],
};

/** A stored report: what the sender's client uploaded. */
const STORED = {
  payload: {
    version: "0.1.0",
    os: "linux",
    arch: "x86_64",
    events: [
      {
        ts_unix_ms: NOW - 3_600_400,
        level: "INFO",
        target: "bookclerkd::daemon",
        message: "listening on 127.0.0.1:8787",
      },
      {
        ts_unix_ms: NOW - 3_600_300,
        level: "WARN",
        target: "bookclerk_library::scan",
        message: "account token expires soon",
      },
      {
        ts_unix_ms: NOW - 3_600_200,
        level: "ERROR",
        target: "bookclerk_library::scan",
        message: "account token expired",
      },
      {
        ts_unix_ms: NOW - 3_600_100,
        level: "DEBUG",
        target: "bookclerk_plugin_host::spawn",
        message: "guest exited",
      },
    ],
  },
};

/**
 * Renders each page from the fixed sample data.
 *
 * @returns The document for each VISUAL_PAGES path.
 */
function renderers(): Record<string, () => SafeHtml> {
  const reports = [0, 1, 2, 3].map(report);
  const url = (path: string): URL => new URL(path, "https://diagnostics.example.org");
  return {
    "/": () => signInPage(CFG, null, "https://example.org/privacy"),
    "/message": () =>
      messagePage(
        "Not a member",
        html`<p>Only members of the ${CFG.owner} organization can sign in.</p>`,
      ),
    "/lookup": () => lookupFormPage(),
    "/lookup/result": () =>
      lookupResultPage({
        reportId: IDS[0] ?? "",
        code: "K7QM-3XRD-PW9T-H2CF",
        receivedAt: NOW - 3_600_000,
        deletedAt: null,
        stored: STORED,
        issues: [
          {
            number: 412,
            title: ISSUE.title,
            state: "open",
            url: "https://github.com/example-org/bookclerk/issues/412",
          },
        ],
        fix: {
          prNumber: 415,
          prUrl: "https://github.com/example-org/bookclerk/pull/415",
          fixedIn: "0.1.3",
        },
      }),
    "/lookup/deleted": () => lookupDeletedPage(IDS[0] ?? ""),
    "/lookup/message": () =>
      lookupMessagePage("Report not found", "No report matches that ID and access code."),
    "/dash": () =>
      overviewPage({
        cfg: CFG,
        member: MEMBER,
        url: url("/dash"),
        now: NOW,
        rangeId: "14d",
        filter: {},
        stats: { total: 42, last24h: 5, prev24h: 3, needsTriage: 2, openIssues: 4 },
        counts: Array.from({ length: 14 }, (_, i) => [
          { bucket: TODAY - i, trigger: "error_burst", n: (i * 3) % 5 },
          { bucket: TODAY - i, trigger: "warn_burst", n: i % 3 },
          { bucket: TODAY - i, trigger: "manual", n: i % 2 },
        ]).flat(),
        platforms: [
          { key: "linux/x86_64", n: 30, errors: 1.4, warnings: 2.1 },
          { key: "macos/aarch64", n: 12, errors: 0.8, warnings: 1.2 },
        ],
        versions: [
          { key: "0.1.3", n: 20, errors: 0.5, warnings: 1 },
          { key: "0.1.2", n: 22, errors: 1.8, warnings: 2.4 },
        ],
        signatures: [
          {
            signature: "a1b2c3d4e5f60718",
            label: reports[0]?.signature_label ?? "",
            n: 9,
            days: DAYS,
            issue: { number: 412, state: "open" },
          },
        ],
        queue: reports.filter((r) => r.issue_number === null),
        afterFix: new Map([[412, FIX]]),
        budget: { spent: 1200, cap: 10_000, week: 3400 },
        versionOptions: ["0.1.3", "0.1.2"],
        capReached: false,
        usage: {
          weekly: 18,
          monthly: 31,
          byKind: [
            { kind: "daemon", n: 14 },
            { kind: "desktop", n: 4 },
          ],
          byVersion: [
            { version: "0.1.3", n: 11 },
            { version: "0.1.2", n: 7 },
          ],
        },
        access: [
          {
            at: NOW - 600_000,
            login: "octo-owner",
            action: "view report",
            report_id: IDS[0] ?? null,
          },
        ],
        reindexNote: "",
        totalSeries: Array.from({ length: 14 }, (_, i) => (i * 5) % 7),
      }),
    "/reports": () =>
      reportsPage({
        cfg: CFG,
        member: MEMBER,
        url: url("/reports"),
        rangeId: "14d",
        filter: {},
        options: {
          versions: ["0.1.3", "0.1.2"],
          platforms: ["linux/x86_64", "macos/aarch64"],
          triggers: ["error_burst", "warn_burst", "manual"],
        },
        page: 1,
        pageSize: 25,
        rows: reports,
        total: reports.length,
      }),
    "/report": () =>
      reportPage({
        cfg: CFG,
        member: MEMBER,
        report: report(0),
        stored: STORED,
        issues: [{ ...ISSUE, source: "filed" }],
        prs: [PR],
        fix: { ...FIX, issue: 412 },
        regression: false,
        similar: reports.slice(1).map(({ report_id, received_at, version, os, arch }) => ({
          report_id,
          received_at,
          version,
          os,
          arch,
        })),
        sigDays: Array.from({ length: 14 }, (_, i) => ({
          label: `Jan ${i + 2}`,
          n: (i * 5) % 7,
          after: i >= 12,
        })),
        sigMarker: { index: 12, label: "#415 merged" },
        openIssues: [{ number: 412, title: ISSUE.title }],
        signatureIssues: [412],
        mute: null,
        installs: 6,
        triageNote: null,
        proposal: null,
        now: NOW,
      }),
    "/issues": () =>
      issuesPage({
        cfg: CFG,
        member: MEMBER,
        now: NOW,
        state: "open",
        issues: [
          ISSUE_ROW,
          { ...ISSUE_ROW, number: 409, title: "Cover download times out", prs: [], reports: 2 },
        ],
        afterFix: new Map([[412, FIX]]),
        syncedAt: NOW - 1_800_000,
      }),
    "/issue": () =>
      issuePage({
        cfg: CFG,
        member: MEMBER,
        now: NOW,
        detail: {
          issue: ISSUE,
          prs: [PR],
          reports: 9,
          installs: 6,
          first_seen: NOW - 6 * DAY_MS,
          last_seen: NOW - 3_600_000,
          versions: [
            { key: "0.1.2", n: 6 },
            { key: "0.1.3", n: 3 },
          ],
          platforms: [
            { key: "linux/x86_64", n: 7 },
            { key: "macos/aarch64", n: 2 },
          ],
          days: DAYS,
          links: reports.slice(0, 2).map((r) => ({
            report_id: r.report_id,
            received_at: r.received_at,
            version: r.version,
            os: r.os,
            arch: r.arch,
            signature: r.signature,
            source: "filed",
            linked_by: null,
            linked_at: null,
            tracked: 1,
          })),
          suggested: [],
          outdated: true,
          pending: 2,
          summarySentAt: NOW - 2 * DAY_MS,
        },
        fix: FIX,
        updateNote: null,
        openIssues: [{ number: 409, title: "Cover download times out" }],
      }),
    "/demo": () =>
      demoPage({
        member: MEMBER,
        now: NOW,
        days: 30,
        stats: {
          live: [
            {
              started_at: NOW / 1000 - 600,
              ends_at: NOW / 1000 + 1200,
              version: "0.1.3",
              startup_seconds: 4,
            },
          ],
          days: Array.from({ length: 10 }, (_, i) => ({
            day: TODAY - 9 + i,
            runs: 3 + (i % 4),
            byReason: { visitor: 2, idle: 1 + (i % 2), time: i % 3 === 0 ? 1 : 0 },
            seconds: 1800 + i * 120,
            timed: 3,
            startupSum: 12 + i,
            startups: 3,
          })),
        },
      }),
  };
}

/** Content types of the static files. */
const TYPES: Readonly<Record<string, string>> = {
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
};

/**
 * Serves the pages until SIGTERM or SIGINT.
 *
 * @throws {Error} When VISUAL_PAGES and the renderers disagree, or the port
 *   is not a TCP port.
 */
function main(): void {
  const { values } = parseArgs({ options: { port: { type: "string", default: "6108" } } });
  const port = Number(values.port);
  if (!(Number.isInteger(port) && port > 0 && port < 65536))
    throw new Error(`--port must be a TCP port, got ${JSON.stringify(values.port)}`);
  const pages = renderers();
  const listed = VISUAL_PAGES.map((p) => p.path).sort();
  const rendered = Object.keys(pages).sort();
  if (listed.join("\n") !== rendered.join("\n"))
    throw new Error(
      `visual/pages.ts lists ${listed.join(", ")} but serve.ts renders ${rendered.join(", ")}`,
    );
  const staticRoot = realpathSync(STATIC_DIR);
  const server = createServer((req, res) => {
    const path = new URL(req.url ?? "/", "http://localhost").pathname;
    const page = Object.hasOwn(pages, path) ? pages[path] : undefined;
    if (page !== undefined) {
      res.writeHead(200, { ...SECURITY_HEADERS, "Content-Type": "text/html; charset=utf-8" });
      res.end(page().toString());
      return;
    }
    if (path.startsWith("/static/")) {
      const file = resolve(staticRoot, decodeURIComponent(path.slice("/static/".length)));
      if (file.startsWith(staticRoot + sep) && existsSync(file) && realpathSync(file) === file) {
        res.writeHead(200, { "Content-Type": TYPES[extname(file)] ?? "application/octet-stream" });
        res.end(readFileSync(file));
        return;
      }
    }
    res.writeHead(404, { "Content-Type": "text/plain; charset=utf-8" }).end("not found");
  });
  server.listen(port, "127.0.0.1", () => console.log(`serving http://127.0.0.1:${port}`));
  for (const signal of ["SIGTERM", "SIGINT"] as const)
    process.on(signal, () => server.close(() => process.exit(0)));
}

main();
