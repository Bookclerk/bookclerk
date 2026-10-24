/**
 * Dashboard page templates. Pure functions from query results to markup;
 * every value is interpolated through `html`, which escapes it.
 */

import { histogram, meter, sparkline, stackedBars } from "./charts.js";
import { RANGES } from "./db.js";
import { fmtDay, fmtInt, fmtTime, html, isoTime, raw } from "./html.js";

/** Stack order and color class for known triggers; others draw as "other". */
export const TRIGGER_SERIES = [
  { key: "error_burst", cls: "fill-brick" },
  { key: "warn_burst", cls: "fill-amber" },
  { key: "manual", cls: "fill-ink" },
  { key: "other", cls: "fill-muted" },
];

/** The Bookclerk mark (assets/brand/svg/bookclerk-mark.svg, unaltered). */
const MARK = raw('<img class="mark" src="/static/mark.svg" width="28" height="28" alt="">');

/**
 * Full HTML document.
 *
 * @param {{title: string, member?: import("./session.js").Member | null,
 *   nav?: string, body: import("./html.js").SafeHtml, bodyClass?: string}} p
 *   Page title, signed-in member (shows the header), active nav item, body.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function layout(p) {
  const navItem = (id, label, href) =>
    html`<a href="${href}"${p.nav === id ? raw(' class="on" aria-current="page"') : ""}>${label}</a>`;
  const header = p.member
    ? html`<header class="band"><div class="wrap band-row">
        <a class="brand" href="/dash">${MARK}<span>Bookclerk</span> <small>Diagnostics</small></a>
        <nav aria-label="Dashboard">${navItem("overview", "Overview", "/dash")}${navItem("reports", "Reports", "/reports")}${navItem("issues", "Issues", "/issues")}</nav>
        <span class="spacer"></span>
        <div class="who"><span>${p.member.login}${p.member.role === "admin" ? " · owner" : ""}</span>
          <form method="post" action="/auth/logout"><button type="submit">Sign out</button></form></div>
      </div></header>`
    : "";
  return html`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex, nofollow">
<title>${p.title} · Bookclerk Diagnostics</title>
<link rel="stylesheet" href="/static/dashboard.css"><link rel="icon" href="/static/mark.svg" type="image/svg+xml">
<script src="/static/dashboard.js" defer></script></head>
<body${p.bodyClass ? html` class="${p.bodyClass}"` : ""}>${header}<main>${p.body}</main></body></html>`;
}

/**
 * A centered card page for sign-in states and errors.
 *
 * @param {string} title Heading.
 * @param {import("./html.js").SafeHtml} body Paragraphs.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function messagePage(title, body) {
  return layout({
    title,
    bodyClass: "center",
    body: html`<div class="card signin">
      <div class="brand ink">${MARK}<span>Bookclerk</span> <small>Diagnostics</small></div>
      <h1>${title}</h1>${body}<a class="button" href="/">Back to sign-in</a></div>`,
  });
}

/**
 * The sign-in page.
 *
 * @param {{owner: string}} cfg Organization shown in the copy.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function signInPage(cfg) {
  return layout({
    title: "Sign in",
    bodyClass: "center",
    body: html`<div class="card signin">
      <div class="brand ink">${MARK}<span>Bookclerk</span> <small>Diagnostics</small></div>
      <h1>Diagnostics reports</h1>
      <p>Reports that Bookclerk users choose to share, grouped with the issues and pull requests that track them. Open to members of the ${cfg.owner} organization on GitHub.</p>
      <a class="button gh" href="/auth/github">${raw(
        '<svg width="20" height="20" viewBox="0 0 16 16" aria-hidden="true"><path fill="currentColor" d="M8 0a8 8 0 0 0-2.53 15.59c.4.07.55-.17.55-.38v-1.33c-2.23.48-2.7-1.07-2.7-1.07-.36-.92-.89-1.17-.89-1.17-.73-.5.06-.49.06-.49.8.06 1.23.83 1.23.83.72 1.22 1.87.87 2.33.66.07-.52.28-.87.5-1.07-1.78-.2-3.65-.89-3.65-3.95 0-.87.31-1.59.83-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82a7.6 7.6 0 0 1 4 0c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.28.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48v2.2c0 .21.15.46.55.38A8 8 0 0 0 8 0Z"/></svg>',
      )}Sign in with GitHub</a>
      <p class="fine">GitHub asks only to confirm who you are. Bookclerk checks your organization membership on its side and does not keep your GitHub token. Sessions last 8 hours.</p>
    </div>`,
  });
}

/**
 * Link to an issue or pull request on GitHub, built from numbers only.
 *
 * @param {{owner: string, repo: string}} cfg Repository.
 * @param {"issues" | "pull"} kind Path segment.
 * @param {number} n Number.
 * @returns {string} URL.
 */
function ghUrl(cfg, kind, n) {
  return `https://github.com/${cfg.owner}/${cfg.repo}/${kind}/${Number(n)}`;
}

/**
 * Chip linking to an issue, or a "Not filed" chip.
 *
 * @param {{owner: string, repo: string}} cfg Repository.
 * @param {number | null | undefined} n Issue number.
 * @param {string | null | undefined} state Issue state.
 * @returns {import("./html.js").SafeHtml} The chip.
 */
function issueChip(cfg, n, state) {
  if (!n) return html`<span class="chip none">Not filed</span>`;
  return html`<a class="chip ${state === "open" ? "open" : "closed"}" href="${ghUrl(cfg, "issues", n)}">#${n}</a>`;
}

/**
 * Range picker links that keep the other query parameters.
 *
 * @param {URL} url Current URL.
 * @param {string} current Active range id.
 * @returns {import("./html.js").SafeHtml} The picker.
 */
function rangePicker(url, current) {
  return html`<nav class="picker" aria-label="Time range">${Object.keys(RANGES).map((id) => {
    const u = new URL(url);
    u.searchParams.set("range", id);
    u.searchParams.delete("page");
    return html`<a href="${u.pathname}${u.search}"${id === current ? raw(' aria-current="true"') : ""}>${RANGES[id].label}</a>`;
  })}</nav>`;
}

/**
 * Bucket labels and values for a range, oldest first.
 *
 * @param {number} now Unix time in milliseconds.
 * @param {{ms: number, bucket: number}} range Range.
 * @param {Map<number, Record<string, number>>} byBucket Bucket index → values.
 * @returns {{label: string, values: Record<string, number>}[]} One per bucket.
 */
function buckets(now, range, byBucket) {
  const last = Math.floor(now / range.bucket);
  const count = Math.round(range.ms / range.bucket);
  const out = [];
  for (let b = last - count + 1; b <= last; b++) {
    const t = b * range.bucket;
    const label = range.bucket < 86_400_000 ? `${String(new Date(t).getUTCHours()).padStart(2, "0")}:00` : fmtDay(t);
    out.push({ label, values: byBucket.get(b) ?? {} });
  }
  return out;
}

/**
 * Daily values for the last `days` days ending today, oldest first.
 *
 * @param {number} now Unix time in milliseconds.
 * @param {number} days Number of days.
 * @param {Map<number, number>} byDay Day index → count.
 * @returns {number[]} Counts.
 */
export function dailySeries(now, days, byDay) {
  const today = Math.floor(now / 86_400_000);
  const out = [];
  for (let d = today - days + 1; d <= today; d++) out.push(byDay.get(d) ?? 0);
  return out;
}

/**
 * Report table rows.
 *
 * @param {{owner: string, repo: string}} cfg Repository.
 * @param {object[]} rows Report rows with issue columns.
 * @param {boolean} full Explorer columns (errors, warnings) rather than the
 *   overview's event count.
 * @returns {import("./html.js").SafeHtml} `<tr>` elements.
 */
function reportRows(cfg, rows, full) {
  return html`${rows.map(
    (r) => html`<tr>
      <td class="num">${fmtTime(r.received_at)}</td>
      <td><a class="mono" href="/reports/${r.report_id}">${String(r.report_id).slice(0, 8)}</a></td>
      <td class="mono">${r.version}</td>
      <td>${`${r.os} ${r.arch}`.trim()}</td>
      <td>${r.trigger}</td>
      <td class="mono wrap-any">${r.signature_label ?? "none"}</td>
      ${full
        ? html`<td class="r num">${r.errors}</td><td class="r num">${r.warnings}</td>`
        : html`<td class="r num">${r.events}</td>`}
      <td>${issueChip(cfg, r.issue_number, r.issue_state)}</td></tr>`,
  )}`;
}

/**
 * The overview page.
 *
 * @param {object} d Query results assembled by routes.js.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function overviewPage(d) {
  const range = RANGES[d.rangeId];
  const byBucket = new Map();
  for (const row of d.counts) {
    const key = TRIGGER_SERIES.some((s) => s.key === row.trigger && s.key !== "other") ? row.trigger : "other";
    const vals = byBucket.get(Number(row.bucket)) ?? {};
    vals[key] = (vals[key] ?? 0) + Number(row.n);
    byBucket.set(Number(row.bucket), vals);
  }
  const series = TRIGGER_SERIES.filter((s) => d.counts.some((c) => (s.key === "other" ? !TRIGGER_SERIES.some((t) => t.key === c.trigger) : c.trigger === s.key)));
  const delta = d.stats.last24h - d.stats.prev24h;
  const platMax = Math.max(1, ...d.platforms.map((p) => Number(p.n)));
  const verMax = Math.max(1, ...d.versions.map((p) => Number(p.n)));
  const afterFixIssues = [...d.afterFix.values()].filter((a) => a.after > 0).length;
  const budgetFrac = d.budget.cap > 0 ? d.budget.spent / d.budget.cap : 0;
  const versionForm = html`<form class="inline" method="get" action="/dash">
      <input type="hidden" name="range" value="${d.rangeId}">
      <label class="sr" for="ov-version">Version</label>
      <select id="ov-version" name="version" class="sel autosubmit"><option value="">All versions</option>${d.versionOptions.map(
        (v) => html`<option value="${v}"${v === d.filter.version ? raw(" selected") : ""}>${v}</option>`,
      )}</select><noscript><button type="submit" class="sel">Apply</button></noscript></form>`;
  const owner = d.member.role === "admin"
    ? html`<section class="panel s12" aria-labelledby="owner-h"><h2 id="owner-h">Owner tools</h2>
        <div class="owner-row">
          <form method="post" action="/admin/reindex"><button class="button small" type="submit">Index older reports from storage</button></form>
          <form method="post" action="/admin/sign-out-all"><button class="button small danger" type="submit">Sign out everyone</button></form>
          <span class="fine">${d.reindexNote}</span>
        </div>
        <div class="tbl"><table><thead><tr><th>Time (UTC)</th><th>Member</th><th>Action</th><th>Report</th></tr></thead><tbody>${d.access.map(
          (a) => html`<tr><td class="num">${fmtTime(a.at)}</td><td>${a.login}</td><td>${a.action}</td><td>${a.report_id ? html`<a class="mono" href="/reports/${a.report_id}">${String(a.report_id).slice(0, 8)}</a>` : ""}</td></tr>`,
        )}</tbody></table></div></section>`
    : "";
  return layout({
    title: "Overview",
    member: d.member,
    nav: "overview",
    body: html`<div class="wrap">
      ${d.capReached ? html`<p class="banner">Today's indexing limit is reached, so new reports are stored but not shown here until tomorrow's reindex.</p>` : ""}
      <div class="toolbar"><h1>Overview</h1>${versionForm}${rangePicker(d.url, d.rangeId)}</div>
      <div class="grid">
        <section class="panel stat s2"><h2>Reports, 24h</h2><div class="v">${fmtInt(d.stats.last24h)}</div><div class="d">${delta >= 0 ? "+" : "−"}${fmtInt(Math.abs(delta))} on the day before</div></section>
        <section class="panel stat s2"><h2>Reports, ${range.label}</h2><div class="v">${fmtInt(d.stats.total)}</div><div class="d">from ${fmtInt(d.stats.installs)} installs</div>${sparkline(d.totalSeries, "teal", 120, 28, true)}</section>
        <section class="panel stat warn s2"><h2>Not filed</h2><div class="v">${fmtInt(d.stats.notFiled)}</div><div class="d">no issue lists them</div></section>
        <section class="panel stat s2"><h2>Open issues</h2><div class="v">${fmtInt(d.stats.openIssues)}</div><div class="d">labeled diagnostics</div></section>
        <section class="panel stat${afterFixIssues ? " crit" : ""} s2"><h2>After a fix</h2><div class="v">${fmtInt(afterFixIssues)}</div><div class="d">issues still reported after their PR merged</div></section>
        <section class="panel stat s2"><h2>Triage budget</h2><div class="v small">${fmtInt(d.budget.spent)}<span class="of"> / ${fmtInt(d.budget.cap)}</span></div>${meter(budgetFrac, "fill-amber", `${Math.round(budgetFrac * 100)} percent of today's neurons used`)}<div class="d">neurons used today (UTC)</div></section>

        <section class="panel s8"><h2>Reports per ${range.bucket < 86_400_000 ? "hour" : "day"}, by trigger <span class="legend">${series.map((s) => html`<span><i class="${s.cls}"></i>${s.key}</span>`)}</span></h2>
          ${stackedBars(buckets(d.now, range, byBucket), series.length ? series : TRIGGER_SERIES.slice(0, 1), "Stacked bars of reports per time bucket by trigger")}</section>
        <section class="panel s4"><h2>Platforms</h2><div class="tbl"><table><thead><tr><th>OS / arch</th><th class="r">Reports</th><th><span class="sr">Share</span></th></tr></thead><tbody>${d.platforms.map(
          (p) => html`<tr><td>${String(p.key).trim() || "unknown"}</td><td class="r num">${fmtInt(p.n)}</td><td class="bar-cell">${meter(Number(p.n) / platMax)}</td></tr>`,
        )}</tbody></table></div></section>

        <section class="panel s7"><h2>Top error signatures <a href="/reports?range=${d.rangeId}">Explore</a></h2><div class="tbl"><table><thead><tr><th>Signature</th><th class="r">${range.label}</th><th>Trend</th><th>Tracked by</th></tr></thead><tbody>${d.signatures.map(
          (s) => html`<tr><td class="mono wrap-any"><a class="quiet" href="/reports?range=${d.rangeId}&amp;signature=${s.signature}">${s.label}</a></td><td class="r num">${fmtInt(s.n)}</td><td>${sparkline(dailySeries(d.now, 14, s.days), s.issue ? "muted" : "amber")}</td><td>${issueChip(d.cfg, s.issue?.number, s.issue?.state)}</td></tr>`,
        )}</tbody></table></div></section>
        <section class="panel s5"><h2>Versions</h2><div class="tbl"><table><thead><tr><th>Version</th><th class="r">Reports</th><th class="r">Errors / report</th><th><span class="sr">Share</span></th></tr></thead><tbody>${d.versions.map(
          (v) => html`<tr><td class="mono">${String(v.key) || "unknown"}</td><td class="r num">${fmtInt(v.n)}</td><td class="r num">${Number(v.errors ?? 0).toFixed(1)}</td><td class="bar-cell">${meter(Number(v.n) / verMax)}</td></tr>`,
        )}</tbody></table></div></section>

        <section class="panel s12"><h2>Recent reports <a href="/reports?range=${d.rangeId}">All reports</a></h2><div class="tbl"><table><thead><tr><th>Received (UTC)</th><th>Report</th><th>Version</th><th>Platform</th><th>Trigger</th><th>Top error</th><th class="r">Events</th><th>Issue</th></tr></thead><tbody>${reportRows(d.cfg, d.recent, false)}</tbody></table></div></section>
        ${owner}
      </div></div>`,
  });
}

/**
 * The reports explorer.
 *
 * @param {object} d Query results assembled by routes.js.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function reportsPage(d) {
  const sel = (id, name, label, options, value) => html`<label class="sr" for="${id}">${label}</label>
    <select id="${id}" name="${name}" class="sel autosubmit">${options.map(
      ([v, text]) => html`<option value="${v}"${v === value ? raw(" selected") : ""}>${text}</option>`,
    )}</select>`;
  const pages = Math.max(1, Math.ceil(d.total / d.pageSize));
  const pageLink = (p, text) => {
    const u = new URL(d.url);
    u.searchParams.set("page", String(p));
    return html`<a class="button small" href="${u.pathname}${u.search}">${text}</a>`;
  };
  return layout({
    title: "Reports",
    member: d.member,
    nav: "reports",
    body: html`<div class="wrap">
      <div class="toolbar"><h1>Reports</h1>${rangePicker(d.url, d.rangeId)}</div>
      <section class="panel">
        <form class="filters" method="get" action="/reports">
          <input type="hidden" name="range" value="${d.rangeId}">
          ${d.filter.signature ? html`<input type="hidden" name="signature" value="${d.filter.signature}">` : ""}
          ${sel("f-ver", "version", "Version", [["", "All versions"], ...d.options.versions.map((v) => [v, v])], d.filter.version ?? "")}
          ${sel("f-os", "platform", "Platform", [["", "All platforms"], ...d.options.platforms.map((v) => [v, v])], d.filter.platform ?? "")}
          ${sel("f-trig", "trigger", "Trigger", [["", "All triggers"], ...d.options.triggers.map((v) => [v, v])], d.filter.trigger ?? "")}
          ${sel("f-filed", "filed", "Filed", [["", "Filed or not"], ["no", "Not filed"], ["yes", "Filed"]], d.filter.filed ?? "")}
          <noscript><button class="button small" type="submit">Apply</button></noscript>
          <span class="fine">${fmtInt(d.total)} reports · page ${d.page} of ${pages}${d.filter.signature ? html` · signature <span class="mono">${d.filter.signature}</span> <a href="/reports?range=${d.rangeId}">clear</a>` : ""}</span>
        </form>
        <div class="tbl"><table><thead><tr><th>Received (UTC)</th><th>Report</th><th>Version</th><th>Platform</th><th>Trigger</th><th>Signature</th><th class="r">Errors</th><th class="r">Warnings</th><th>Issue</th></tr></thead>
          <tbody>${d.rows.length ? reportRows(d.cfg, d.rows, true) : html`<tr><td colspan="9" class="empty">No reports match these filters.</td></tr>`}</tbody></table></div>
        <div class="pager">${d.page > 1 ? pageLink(d.page - 1, "Newer") : ""}${d.page < pages ? pageLink(d.page + 1, "Older") : ""}</div>
      </section></div>`,
  });
}

/**
 * A count with its noun, singular for one.
 *
 * @param {number} n Count.
 * @param {string} noun Singular noun.
 * @returns {string} For example `1 error`, `3 errors`.
 */
function plural(n, noun) {
  return `${fmtInt(n)} ${noun}${Number(n) === 1 ? "" : "s"}`;
}

/**
 * Bounded text for an untrusted JSON value: strings as they are, anything
 * else as JSON. Never throws (objects from storage may carry keys such as
 * `toString` that break `String()`).
 *
 * @param {unknown} value Field key or value from storage.
 * @param {number} max Longest result, in characters.
 * @returns {string} The text, or an empty string when it cannot be shown.
 */
function fieldText(value, max) {
  if (typeof value === "string") return value.slice(0, max);
  try {
    return String(JSON.stringify(value) ?? "").slice(0, max);
  } catch {
    return "";
  }
}

/**
 * Event rows of a stored report, bounded. Exported for the fuzz tests: the
 * events are whatever anyone submitted.
 *
 * @param {unknown} events `payload.events` from storage.
 * @returns {{counts: Record<string, number>, rows: import("./html.js").SafeHtml}}
 *   Level counts and `<div>` rows.
 */
export function eventLog(events) {
  const list = Array.isArray(events) ? events.slice(0, 500) : [];
  const counts = { ERROR: 0, WARN: 0, INFO: 0, DEBUG: 0, TRACE: 0 };
  const rows = list.map((ev) => {
    const e = ev && typeof ev === "object" ? ev : {};
    let level = typeof e.level === "string" ? e.level.toUpperCase() : "";
    if (level === "WARNING") level = "WARN";
    if (!Object.hasOwn(counts, level)) level = "INFO";
    counts[level] += 1;
    const ts = isoTime(e.ts_unix_ms).slice(11, 23);
    const fields = Array.isArray(e.fields)
      ? e.fields
          .slice(0, 20)
          .filter((f) => Array.isArray(f) && f.length === 2)
          .map(([k, v]) => `${fieldText(k, 64)}=${fieldText(v, 256)}`)
          .join(" ")
      : "";
    const message = typeof e.message === "string" ? e.message.slice(0, 8000) : "";
    return html`<div class="row lv-${level.toLowerCase()}"><span class="t">${ts}</span><span class="lv">${level}</span><span class="tg">${typeof e.target === "string" ? e.target.slice(0, 256) : ""}</span><span class="msg">${message}${fields ? html` <span class="fields">${fields}</span>` : ""}</span></div>`;
  });
  return { counts, rows: html`${rows}` };
}

/**
 * One report.
 *
 * @param {object} d Query results assembled by routes.js.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function reportPage(d) {
  const r = d.report;
  const p = d.stored?.payload && typeof d.stored.payload === "object" ? d.stored.payload : {};
  const log = eventLog(p.events);
  const title = d.issues[0]?.title ?? r.signature_label ?? "Report";
  const fix = d.fix;
  const filters = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"]
    .filter((l) => log.counts[l] > 0)
    .map(
      (l) => html`<button type="button" class="lvl-toggle" data-level="${l.toLowerCase()}" aria-pressed="${l === "DEBUG" || l === "TRACE" ? "false" : "true"}">${{ ERROR: "Errors", WARN: "Warnings", INFO: "Info", DEBUG: "Debug", TRACE: "Trace" }[l]} ${log.counts[l]}</button>`,
    );
  return layout({
    title: `Report ${String(r.report_id).slice(0, 8)}`,
    member: d.member,
    nav: "reports",
    body: html`<div class="wrap">
      <div class="toolbar column"><span class="crumb"><a href="/reports">Reports</a> / <span class="mono">${r.report_id}</span></span>
        <h1>${title}</h1><span class="fine">Your view of this report is recorded in the access log.</span></div>
      <div class="grid">
        <section class="panel s8"><h2>Report</h2><dl class="meta">
          <div><dt>Received</dt><dd class="num">${fmtTime(r.received_at)} UTC</dd></div>
          <div><dt>Version</dt><dd class="mono">${r.version}</dd></div>
          <div><dt>Platform</dt><dd>${`${r.os} ${r.arch}`.trim()}${r.distro ? `, ${r.distro}` : ""}</dd></div>
          <div><dt>rustc</dt><dd class="mono">${typeof p.rustc_release === "string" && p.rustc_release ? `${p.rustc_release.slice(0, 64)} ${typeof p.rustc_channel === "string" ? p.rustc_channel.slice(0, 32) : ""}` : "not reported"}</dd></div>
          <div><dt>Trigger</dt><dd>${r.trigger}</dd></div>
          <div><dt>Events</dt><dd class="num">${fmtInt(r.events)} (${plural(r.errors, "error")}, ${plural(r.warnings, "warning")})</dd></div>
          <div><dt>Signature</dt><dd class="mono">${r.signature ? html`<a href="/reports?range=90d&amp;signature=${r.signature}">${r.signature}</a>` : "none"}</dd></div>
          <div><dt>Install</dt><dd class="mono">${r.install_hash ? `${String(r.install_hash).slice(0, 4)}…${String(r.install_hash).slice(-4)}` : "not recorded"}</dd></div>
        </dl></section>
        <section class="panel s4"><h2>Tracked by</h2><div class="links">
          ${d.issues.length
            ? d.issues.map((i) => html`<div class="link-item"><span class="chip ${i.state === "open" ? "open" : "closed"}">${i.state === "open" ? "Open" : "Closed"}</span><a href="${ghUrl(d.cfg, "issues", i.number)}">#${i.number} ${i.title}</a></div>`)
            : html`<p class="fine">No issue lists this report yet. Triage may have judged it noise, or ingest has not run since it arrived.</p>`}
          ${d.prs.map((pr) => html`<div class="link-item"><span class="chip ${pr.state}">${{ open: "Open", closed: "Closed", merged: "Merged" }[pr.state] ?? pr.state}</span><a href="${ghUrl(d.cfg, "pull", pr.pr_number)}">#${pr.pr_number} ${pr.title}</a></div>`)}
          ${fix && fix.after > 0 ? html`<div class="link-item"><span class="chip crit">Seen after fix</span><span class="fine">${fmtInt(fix.after)} reports with this signature since the fix merged on ${fmtDay(fix.fixAt)}</span></div>` : ""}
        </div></section>
        <section class="panel s12"><h2>Event log <span class="note">secrets were redacted on the client before upload</span></h2>
          ${d.stored
            ? html`<div class="filters" role="group" aria-label="Levels">${filters}</div><div class="log" id="log" role="log" aria-label="Report events">${log.rows}</div>`
            : html`<p class="banner">The stored report could not be read from storage. The index entry above is all that is available.</p>`}
        </section>
        <section class="panel s6"><h2>Same signature, last 14 days</h2>${r.signature ? histogram(d.sigDays, d.sigMarker, "Reports per day with this signature") : html`<p class="fine">This report has no errors or warnings to group by.</p>`}</section>
        <section class="panel s6"><h2>Similar reports</h2><div class="tbl"><table><thead><tr><th>Received (UTC)</th><th>Report</th><th>Version</th><th>Platform</th></tr></thead><tbody>${d.similar.length
          ? d.similar.map((s) => html`<tr><td class="num">${fmtTime(s.received_at)}</td><td><a class="mono" href="/reports/${s.report_id}">${String(s.report_id).slice(0, 8)}</a></td><td class="mono">${s.version}</td><td>${`${s.os} ${s.arch}`.trim()}</td></tr>`)
          : html`<tr><td colspan="4" class="empty">No other report shares this signature.</td></tr>`}</tbody></table></div></section>
      </div></div>`,
  });
}

/**
 * The issues and fixes board.
 *
 * @param {object} d Query results assembled by routes.js.
 * @returns {import("./html.js").SafeHtml} The document.
 */
export function issuesPage(d) {
  const tab = (id, label) => html`<a href="/issues?state=${id}"${id === d.state ? raw(' aria-current="true"') : ""}>${label}</a>`;
  return layout({
    title: "Issues",
    member: d.member,
    nav: "issues",
    body: html`<div class="wrap">
      <div class="toolbar"><h1>Issues and fixes</h1><nav class="picker" aria-label="Issue state">${tab("open", "Open")}${tab("closed", "Closed")}${tab("all", "All")}</nav></div>
      <section class="panel"><h2>Diagnostics issues <span class="note">${d.syncedAt ? `synced from GitHub ${fmtTime(d.syncedAt)} UTC` : "not synced from GitHub yet"}</span></h2>
        <div class="tbl"><table><thead><tr><th>Issue</th><th>State</th><th class="r">Reports</th><th>Activity, 14d</th><th>First seen</th><th>Last seen</th><th>Fix</th><th class="r">After fix</th></tr></thead><tbody>${d.issues.length
          ? d.issues.map((i) => {
              const fix = d.afterFix.get(Number(i.number));
              const merged = i.prs.find((p) => p.state === "merged");
              const open = i.prs.find((p) => p.state === "open");
              const pr = merged ?? open;
              return html`<tr>
                <td class="issue-cell"><a href="${ghUrl(d.cfg, "issues", i.number)}"><b>#${i.number}</b> ${i.title}</a>${i.kind === "fallback" ? html` <span class="chip none">untriaged batch</span>` : ""}</td>
                <td><span class="chip ${i.state === "open" ? "open" : "closed"}">${i.state === "open" ? "Open" : "Closed"}</span></td>
                <td class="r num">${fmtInt(i.reports)}</td>
                <td>${sparkline(dailySeries(d.now, 14, i.days), fix && fix.after > 0 ? "brick" : "teal")}</td>
                <td class="num">${i.first_seen ? html`${fmtDay(i.first_seen)} · <span class="mono">${i.first_version}</span>` : ""}</td>
                <td class="num">${i.last_seen ? html`${fmtDay(i.last_seen)} · <span class="mono">${i.last_version}</span>` : ""}</td>
                <td>${pr ? html`<a class="chip ${pr.state}" href="${ghUrl(d.cfg, "pull", pr.pr_number)}">PR #${pr.pr_number} ${pr.state}</a>` : html`<span class="fine">none linked</span>`}</td>
                <td class="r num">${!fix ? html`<span class="fine">no fix yet</span>` : fix.after > 0 ? html`<span class="chip crit">${fmtInt(fix.after)} reports</span>` : "0"}</td></tr>`;
            })
          : html`<tr><td colspan="8" class="empty">No diagnostics issues in this view.</td></tr>`}</tbody></table></div>
      </section></div>`,
  });
}
