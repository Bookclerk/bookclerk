/**
 * Queries over the dashboard's D1 database (binding DASHBOARD_DB, schema in
 * migrations/). Every statement binds its values; no SQL is built from
 * request or report text, only from the fixed filter names below.
 */

import { compareSemver, isPackagingDerivative, normalizeVersion } from "../version-filter.js";
import { sha256Hex } from "./session.js";

/** One day, in milliseconds. */
export const DAY_MS = 86_400_000;

/** One hour, in milliseconds. */
export const HOUR_MS = 3_600_000;

/** Time ranges the pages offer: id → length and chart bucket. */
export const RANGES = {
  "24h": { ms: DAY_MS, bucket: HOUR_MS, label: "24h" },
  "14d": { ms: 14 * DAY_MS, bucket: DAY_MS, label: "14d" },
  "30d": { ms: 30 * DAY_MS, bucket: DAY_MS, label: "30d" },
  "90d": { ms: 90 * DAY_MS, bucket: DAY_MS, label: "90d" },
};

/** Range used when the request names none or an unknown one. */
export const DEFAULT_RANGE = "14d";

/**
 * Default daily cap on reports /submit may index. Each indexed report writes
 * five D1 rows (measured with `meta.rows_written`: four for the report row
 * and its indexes, one for the day's ledger), so the cap spends at most half
 * of D1's 100,000 free daily row writes and leaves the rest for sign-ins,
 * the access log, the issue sync and the owner's reindex.
 */
export const DEFAULT_INDEX_DAILY_ROWS = 10_000;

/** Reports per page in the explorer. */
export const PAGE_SIZE = 25;

/**
 * Rows of a statement.
 *
 * @param {D1Database} db Database.
 * @param {string} sql Statement with `?` placeholders.
 * @param {...unknown} params Bound values.
 * @returns {Promise<object[]>} Result rows.
 */
export async function all(db, sql, ...params) {
  const res = await db.prepare(sql).bind(...params).all();
  return res.results ?? [];
}

/**
 * First row of a statement.
 *
 * @param {D1Database} db Database.
 * @param {string} sql Statement with `?` placeholders.
 * @param {...unknown} params Bound values.
 * @returns {Promise<object | null>} The row, or null.
 */
export async function first(db, sql, ...params) {
  return (await db.prepare(sql).bind(...params).first()) ?? null;
}

/**
 * Runs a statement for its effect.
 *
 * @param {D1Database} db Database.
 * @param {string} sql Statement with `?` placeholders.
 * @param {...unknown} params Bound values.
 * @returns {Promise<void>}
 */
export async function run(db, sql, ...params) {
  await db.prepare(sql).bind(...params).run();
}

/**
 * UTC day of a time, `YYYY-MM-DD`.
 *
 * @param {number} ms Unix time in milliseconds.
 * @returns {string} The day.
 */
export function utcDayOf(ms) {
  return new Date(ms).toISOString().slice(0, 10);
}

/**
 * The statement that adds one report row, ignoring a report already indexed.
 * It writes four D1 rows (the row, its primary-key index, `reports_received`
 * and `reports_signature`).
 *
 * @param {D1Database} db Database.
 * @param {{reportId: string, objectKey: string, receivedAt: number,
 *   summary: Awaited<ReturnType<typeof import("./signature.js").summarizeReport>>}} row
 *   The report and its summary.
 * @returns {D1PreparedStatement} The bound statement.
 */
export function reportInsert(db, row) {
  const s = row.summary;
  return db
    .prepare(
      `INSERT OR IGNORE INTO reports (report_id, object_key, received_at, version, os, arch, distro,
         trigger, events, errors, warnings, signature, signature_label, install_hash)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(
      row.reportId,
      row.objectKey,
      row.receivedAt,
      s.version,
      s.os,
      s.arch,
      s.distro,
      s.trigger,
      s.events,
      s.errors,
      s.warnings,
      s.signature,
      s.signatureLabel,
      s.installHash,
    );
}

/**
 * Adds a report to the index unless today's cap is reached.
 *
 * The cap holds under concurrent `/submit` requests: a slot is reserved by
 * one conditional upsert on the day's ledger row (atomic in SQLite and D1)
 * before the report row is written, and handed back when the report turns
 * out to be indexed already.
 *
 * @param {D1Database} db Database.
 * @param {{reportId: string, objectKey: string, receivedAt: number,
 *   summary: Awaited<ReturnType<typeof import("./signature.js").summarizeReport>>}} row
 *   The report and its summary.
 * @param {number} cap Most rows indexed per UTC day (by receipt day); a
 *   positive safe integer.
 * @returns {Promise<"indexed" | "capped">} What happened. A report already
 *   indexed counts as indexed and does not use up the cap.
 */
export async function indexReport(db, row, cap) {
  if (await first(db, "SELECT 1 AS hit FROM reports WHERE report_id = ?", row.reportId)) return "indexed";
  const day = utcDayOf(row.receivedAt);
  const reserved = await db
    .prepare(
      `INSERT INTO index_days (day, rows) VALUES (?, 1)
       ON CONFLICT (day) DO UPDATE SET rows = rows + 1 WHERE rows < ?`,
    )
    .bind(day, cap)
    .run();
  if ((reserved.meta?.changes ?? 0) === 0) return "capped";
  const insert = await reportInsert(db, row).run();
  if ((insert.meta?.changes ?? 0) === 0) {
    // Indexed by a concurrent request in the meantime: hand the slot back.
    await run(db, "UPDATE index_days SET rows = rows - 1 WHERE day = ? AND rows > 0", day);
  }
  return "indexed";
}

/**
 * Whether today's index cap has been reached.
 *
 * @param {D1Database} db Database.
 * @param {number} now Unix time in milliseconds.
 * @param {number} cap Daily cap.
 * @returns {Promise<boolean>} True when /submit has stopped indexing today.
 */
export async function indexCapReached(db, now, cap) {
  const used = await first(db, "SELECT rows FROM index_days WHERE day = ?", utcDayOf(now));
  return Boolean(used && Number(used.rows) >= cap);
}

/**
 * Report filters the explorer and overview accept. Values come from the query
 * string and are only ever bound, never spliced into SQL.
 *
 * @typedef {{version?: string, platform?: string, trigger?: string,
 *   filed?: "" | "yes" | "no", signature?: string}} ReportFilter
 */

/**
 * WHERE clause and values for a time range plus filters.
 *
 * @param {number} since Earliest receipt time, in milliseconds.
 * @param {ReportFilter} f Filters.
 * @returns {{where: string, params: unknown[]}} Clause over alias `r`.
 */
export function reportWhere(since, f = {}) {
  const clauses = ["r.received_at >= ?"];
  const params = [since];
  if (f.version) {
    clauses.push("r.version = ?");
    params.push(f.version);
  }
  if (f.platform) {
    clauses.push("(r.os || ' ' || r.arch) = ?");
    params.push(f.platform);
  }
  if (f.trigger) {
    clauses.push("r.trigger = ?");
    params.push(f.trigger);
  }
  if (f.signature) {
    clauses.push("r.signature = ?");
    params.push(f.signature);
  }
  if (f.filed === "yes" || f.filed === "no") {
    clauses.push(`${f.filed === "no" ? "NOT " : ""}EXISTS (SELECT 1 FROM report_issue ri WHERE ri.report_id = r.report_id AND ${LINKED})`);
  }
  return { where: clauses.join(" AND "), params };
}

/**
 * Condition on a `report_issue` row that counts as a link: every source but
 * `excluded`, which records that a member detached the report.
 */
const LINKED = "ri.source <> 'excluded'";

/**
 * Order of a report's links when choosing the issue that tracks it: a
 * triaged issue before the holding issue for untriaged reports, then the
 * oldest. One link is the normal case; two triaged issues mean the report
 * was filed twice, and the older keeps it until a member resolves the
 * conflict; a held report that a real issue comes to list moves to it.
 */
const TRACKER_ORDER = "(COALESCE(ti.kind, '') = 'fallback'), ri.issue_number";

/** The issue that tracks a report (alias `r`), by `TRACKER_ORDER`. */
const TRACKER = `(SELECT ri.issue_number FROM report_issue ri LEFT JOIN issues ti ON ti.number = ri.issue_number
  WHERE ri.report_id = r.report_id AND ${LINKED} ORDER BY ${TRACKER_ORDER} LIMIT 1)`;

/**
 * Columns for a report row with its tracking issue, how that link was made
 * and how many issues link it in all, so a list can flag a conflict.
 */
const REPORT_COLUMNS = `r.report_id, r.object_key, r.received_at, r.version, r.os, r.arch, r.distro,
  r.trigger, r.events, r.errors, r.warnings, r.signature, r.signature_label, r.install_hash,
  i.number AS issue_number, i.state AS issue_state,
  (SELECT ri.source FROM report_issue ri WHERE ri.report_id = r.report_id AND ri.issue_number = i.number) AS issue_source,
  (SELECT COUNT(*) FROM report_issue ri WHERE ri.report_id = r.report_id AND ${LINKED}) AS issue_count`;

/** Join to the issue that tracks a report. */
const ISSUE_JOIN = `LEFT JOIN issues i ON i.number = ${TRACKER}`;

/**
 * Whether two versions are on the same `major.minor` line, the rule for
 * linking a report to an issue by signature alone: an unrelated failure in
 * a later line with the same first error should reach triage with the old
 * issue as a candidate, not inherit it.
 *
 * @param {unknown} a One version string.
 * @param {unknown} b Another.
 * @returns {boolean} True when both parse and their lines are equal.
 */
export function sameVersionLine(a, b) {
  const line = (v) => (typeof v === "string" ? /^v?(\d+)\.(\d+)(?:[.+-]|$)/.exec(v) : null);
  const x = line(a);
  const y = line(b);
  return Boolean(x && y && x[1] === y[1] && x[2] === y[2]);
}

/** Report IDs looked up per statement when linking by signature. */
const LINK_LOOKUP_CHUNK = 90;

/**
 * Links reports to the open issue that already tracks their error
 * signature, source `signature`. A report qualifies when it is indexed with
 * a signature, is linked to no issue yet, and an open, triaged issue (not
 * the holding issue for untriaged reports) tracks a report with the same
 * signature on the same `major.minor` version line, and a member has not
 * excluded the report from that issue; with several such issues the oldest
 * wins. Writes nothing for reports that do not qualify.
 *
 * @param {D1Database} db Database.
 * @param {string[]} reportIds Lowercase report UUIDs pulled by ingest.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<Map<string, number>>} Report ID → issue number linked.
 */
export async function linkBySignature(db, reportIds, now) {
  const chosen = new Map();
  for (let i = 0; i < reportIds.length; i += LINK_LOOKUP_CHUNK) {
    const chunk = reportIds.slice(i, i + LINK_LOOKUP_CHUNK);
    // One row per (report, tracked version, issue), however many reports
    // share the signature; the version-line rule needs each version once.
    const rows = await all(
      db,
      `SELECT DISTINCT r.report_id AS report_id, r.version AS version, t.version AS tracked_version,
         ri.issue_number AS issue
       FROM reports r
       JOIN reports t ON t.signature = r.signature AND t.report_id <> r.report_id
       JOIN report_issue ri ON ri.report_id = t.report_id AND ${LINKED}
       JOIN issues i ON i.number = ri.issue_number AND i.state = 'open' AND i.kind <> 'fallback'
       WHERE r.signature IS NOT NULL AND r.report_id IN (${chunk.map(() => "?").join(", ")})
         AND NOT EXISTS (SELECT 1 FROM report_issue x WHERE x.report_id = r.report_id AND x.source <> 'excluded')
         AND NOT EXISTS (SELECT 1 FROM report_issue x WHERE x.report_id = r.report_id
           AND x.issue_number = ri.issue_number AND x.source = 'excluded')`,
      ...chunk,
    );
    for (const row of rows) {
      if (!sameVersionLine(row.version, row.tracked_version)) continue;
      const issue = Number(row.issue);
      const id = String(row.report_id);
      if (!chosen.has(id) || issue < chosen.get(id)) chosen.set(id, issue);
    }
  }
  if (chosen.size) {
    await db.batch(
      [...chosen].map(([id, issue]) =>
        db
          .prepare(
            `INSERT OR IGNORE INTO report_issue (report_id, issue_number, source, linked_by, linked_at)
             VALUES (?, ?, 'signature', 'ingest', ?)`,
          )
          .bind(id, issue, now),
      ),
    );
  }
  return chosen;
}

/**
 * Sorts reports no issue links yet against the closed issues that tracked
 * their error signature: a report from the version that fixed such an issue
 * or a later one (`isRegressionVersion`) is a regression, for ingest to
 * open a new issue over; one from an older version on the same
 * `major.minor` line as a report the closed issue tracked is a known
 * failure in an old build and is linked to that issue quietly (source
 * `signature`, as `linkBySignature`), so it does not reach triage. A closed
 * issue without a fixed-in version (no fix, or one not released yet) can
 * only take the quiet link. With several closed issues the regression goes
 * back to the one with the newest fixed-in version, the quiet link to the
 * oldest issue; a member's exclusion of the pair is honored. Writes nothing
 * for reports that match neither way.
 *
 * @param {D1Database} db Database.
 * @param {string[]} reportIds Lowercase report UUIDs pulled by ingest and
 *   not linked by `linkBySignature`.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<{regressions: Map<string, {issue: number, fixedIn: string,
 *   prNumber: number | null, title: string}>, linked: Map<string, number>}>}
 *   Report ID → the closed issue it regresses, and report ID → the closed
 *   issue it was linked to.
 */
export async function matchClosedBySignature(db, reportIds, now) {
  const regressions = new Map();
  const linked = new Map();
  for (let i = 0; i < reportIds.length; i += LINK_LOOKUP_CHUNK) {
    const chunk = reportIds.slice(i, i + LINK_LOOKUP_CHUNK);
    const rows = await all(
      db,
      `WITH ${LATEST_FIX}
       SELECT DISTINCT r.report_id AS report_id, r.version AS version, t.version AS tracked_version,
         ri.issue_number AS issue, i.title AS title, l.fixed_in AS fixed_in, l.pr_number AS pr_number
       FROM reports r
       JOIN reports t ON t.signature = r.signature AND t.report_id <> r.report_id
       JOIN report_issue ri ON ri.report_id = t.report_id AND ${LINKED}
       JOIN issues i ON i.number = ri.issue_number AND i.state = 'closed' AND i.kind <> 'fallback'
       LEFT JOIN latest l ON l.issue_number = i.number
       WHERE r.signature IS NOT NULL AND r.report_id IN (${chunk.map(() => "?").join(", ")})
         AND NOT EXISTS (SELECT 1 FROM report_issue x WHERE x.report_id = r.report_id AND x.source <> 'excluded')
         AND NOT EXISTS (SELECT 1 FROM report_issue x WHERE x.report_id = r.report_id
           AND x.issue_number = ri.issue_number AND x.source = 'excluded')`,
      ...chunk,
    );
    const byReport = new Map();
    for (const row of rows) {
      const id = String(row.report_id);
      if (!byReport.has(id)) byReport.set(id, []);
      byReport.get(id).push(row);
    }
    for (const [id, matches] of byReport) {
      const version = String(matches[0].version ?? "");
      // A closed issue on the same line with no fix recorded was closed by a
      // decision (not a bug, will not fix, or a regression issue a member
      // closed): the report joins it quietly rather than reopening the
      // question with yet another regression issue.
      const decided = matches.some((m) => (m.fixed_in === null || m.fixed_in === undefined) && sameVersionLine(version, m.tracked_version));
      let regression = null;
      for (const m of matches) {
        if (decided) break;
        const fixedIn = m.fixed_in === null || m.fixed_in === undefined ? null : String(m.fixed_in);
        if (fixedIn === null || !isRegressionVersion(version, fixedIn)) continue;
        const newer = regression === null || (compareSemver(fixedIn, regression.fixedIn) ?? 0) > 0
          || ((compareSemver(fixedIn, regression.fixedIn) ?? 0) === 0 && Number(m.issue) > regression.issue);
        if (newer) {
          regression = {
            issue: Number(m.issue),
            fixedIn,
            prNumber: m.pr_number === null || m.pr_number === undefined ? null : Number(m.pr_number),
            title: String(m.title ?? ""),
          };
        }
      }
      if (regression) {
        regressions.set(id, regression);
        continue;
      }
      for (const m of matches) {
        if (!sameVersionLine(version, m.tracked_version)) continue;
        const unfixed = m.fixed_in === null || m.fixed_in === undefined;
        if (decided && !unfixed) continue;
        const issue = Number(m.issue);
        // The newest decision wins; among fixed issues the oldest, as for open ones.
        const better = !linked.has(id) || (decided ? issue > linked.get(id) : issue < linked.get(id));
        if (better) linked.set(id, issue);
      }
    }
  }
  if (linked.size) {
    await db.batch(
      [...linked].map(([id, issue]) =>
        db
          .prepare(
            `INSERT OR IGNORE INTO report_issue (report_id, issue_number, source, linked_by, linked_at)
             VALUES (?, ?, 'signature', 'ingest', ?)`,
          )
          .bind(id, issue, now),
      ),
    );
  }
  return { regressions, linked };
}

/**
 * Overview figures for a range.
 *
 * @param {D1Database} db Database.
 * @param {number} now Unix time in milliseconds.
 * @param {number} since Range start, in milliseconds.
 * @param {ReportFilter} f Filters (version only on the overview).
 * @returns {Promise<{total: number, installs: number, last24h: number,
 *   prev24h: number, notFiled: number, openIssues: number}>} The figures.
 */
export async function overviewStats(db, now, since, f) {
  const range = reportWhere(since, f);
  const d1 = reportWhere(now - DAY_MS, f);
  const d2 = reportWhere(now - 2 * DAY_MS, f);
  const [tot, a, b, nf, oi] = await Promise.all([
    first(db, `SELECT COUNT(*) AS n, COUNT(DISTINCT NULLIF(r.install_hash, '')) AS installs FROM reports r WHERE ${range.where}`, ...range.params),
    first(db, `SELECT COUNT(*) AS n FROM reports r WHERE ${d1.where}`, ...d1.params),
    first(db, `SELECT COUNT(*) AS n FROM reports r WHERE ${d2.where} AND r.received_at < ?`, ...d2.params, now - DAY_MS),
    first(db, `SELECT COUNT(*) AS n FROM reports r WHERE ${range.where} AND NOT EXISTS (SELECT 1 FROM report_issue ri WHERE ri.report_id = r.report_id AND ${LINKED})`, ...range.params),
    first(db, "SELECT COUNT(*) AS n FROM issues WHERE state = 'open'"),
  ]);
  return {
    total: Number(tot?.n ?? 0),
    installs: Number(tot?.installs ?? 0),
    last24h: Number(a?.n ?? 0),
    prev24h: Number(b?.n ?? 0),
    notFiled: Number(nf?.n ?? 0),
    openIssues: Number(oi?.n ?? 0),
  };
}

/**
 * Reports per time bucket and trigger.
 *
 * @param {D1Database} db Database.
 * @param {number} since Range start, in milliseconds.
 * @param {number} bucket Bucket length, in milliseconds.
 * @param {ReportFilter} f Filters.
 * @returns {Promise<{bucket: number, trigger: string, n: number}[]>} Bucket
 *   index (time / bucket, floored) per row.
 */
export async function countsByBucket(db, since, bucket, f) {
  const w = reportWhere(since, f);
  return all(
    db,
    `SELECT CAST(r.received_at / ? AS INTEGER) AS bucket, r.trigger AS trigger, COUNT(*) AS n
     FROM reports r WHERE ${w.where} GROUP BY bucket, r.trigger ORDER BY bucket`,
    bucket,
    ...w.params,
  );
}

/**
 * Top values of a grouping column in a range.
 *
 * @param {D1Database} db Database.
 * @param {"platform" | "version" | "trigger"} by Grouping.
 * @param {number} since Range start, in milliseconds.
 * @param {ReportFilter} f Filters.
 * @param {number} limit Most rows.
 * @returns {Promise<{key: string, n: number, errors: number}[]>} Rows by
 *   count, with the mean errors per report.
 */
export async function topBy(db, by, since, f, limit) {
  const expr = { platform: "(r.os || ' ' || r.arch)", version: "r.version", trigger: "r.trigger" }[by];
  const w = reportWhere(since, f);
  return all(
    db,
    `SELECT ${expr} AS key, COUNT(*) AS n, AVG(r.errors) AS errors
     FROM reports r WHERE ${w.where} GROUP BY key ORDER BY n DESC, key LIMIT ?`,
    ...w.params,
    limit,
  );
}

/**
 * Most frequent signatures in a range, each with its daily counts and the
 * newest issue tracking any report with it.
 *
 * @param {D1Database} db Database.
 * @param {number} since Range start, in milliseconds.
 * @param {ReportFilter} f Filters.
 * @param {number} limit Most signatures.
 * @returns {Promise<{signature: string, label: string, n: number,
 *   days: Map<number, number>, issue: {number: number, state: string} | null}[]>}
 *   Signatures by count.
 */
export async function topSignatures(db, since, f, limit) {
  const w = reportWhere(since, f);
  const sigs = await all(
    db,
    `SELECT r.signature AS signature, MAX(r.signature_label) AS label, COUNT(*) AS n
     FROM reports r WHERE ${w.where} AND r.signature IS NOT NULL
     GROUP BY r.signature ORDER BY n DESC LIMIT ?`,
    ...w.params,
    limit,
  );
  return Promise.all(
    sigs.map(async (s) => {
      const [days, issue] = await Promise.all([
        all(
          db,
          `SELECT CAST(r.received_at / ? AS INTEGER) AS day, COUNT(*) AS n FROM reports r
           WHERE ${w.where} AND r.signature = ? GROUP BY day`,
          DAY_MS,
          ...w.params,
          s.signature,
        ),
        first(
          db,
          `SELECT i.number AS number, i.state AS state FROM reports r
           JOIN report_issue ri ON ri.report_id = r.report_id AND ${LINKED}
           JOIN issues i ON i.number = ri.issue_number
           WHERE r.signature = ? ORDER BY i.updated_at DESC LIMIT 1`,
          s.signature,
        ),
      ]);
      return {
        signature: s.signature,
        label: s.label ?? "",
        n: Number(s.n),
        days: new Map(days.map((d) => [Number(d.day), Number(d.n)])),
        issue: issue ? { number: Number(issue.number), state: String(issue.state) } : null,
      };
    }),
  );
}

/**
 * Reports in a range, newest first, with their newest linked issue.
 *
 * @param {D1Database} db Database.
 * @param {number} since Range start, in milliseconds.
 * @param {ReportFilter} f Filters.
 * @param {number} limit Rows per page.
 * @param {number} offset Rows skipped.
 * @returns {Promise<{rows: object[], total: number}>} One page and the
 *   number of matching reports.
 */
export async function listReports(db, since, f, limit, offset) {
  const w = reportWhere(since, f);
  const [rows, count] = await Promise.all([
    all(
      db,
      `SELECT ${REPORT_COLUMNS} FROM reports r ${ISSUE_JOIN}
       WHERE ${w.where} ORDER BY r.received_at DESC, r.report_id LIMIT ? OFFSET ?`,
      ...w.params,
      limit,
      offset,
    ),
    first(db, `SELECT COUNT(*) AS n FROM reports r WHERE ${w.where}`, ...w.params),
  ]);
  return { rows, total: Number(count?.n ?? 0) };
}

/**
 * Values offered in the explorer's filter menus for a range.
 *
 * @param {D1Database} db Database.
 * @param {number} since Range start, in milliseconds.
 * @returns {Promise<{versions: string[], platforms: string[], triggers: string[]}>}
 *   Up to 50 of each, most frequent first.
 */
export async function filterOptions(db, since) {
  const [v, p, t] = await Promise.all(
    ["version", "platform", "trigger"].map((by) => topBy(db, by, since, {}, 50)),
  );
  const keys = (rows) => rows.map((r) => String(r.key)).filter((k) => k.trim() !== "");
  return { versions: keys(v), platforms: keys(p), triggers: keys(t) };
}

/**
 * One indexed report with its linked issues (the tracking issue first, in the
 * same order the counts use: triaged issues before the holding issue, then
 * oldest first; each with the link's `source`) and their pull requests.
 *
 * @param {D1Database} db Database.
 * @param {string} reportId Lowercase UUID.
 * @returns {Promise<{report: object, issues: object[], prs: object[]} | null>}
 *   Null when the report is not indexed.
 */
export async function getReport(db, reportId) {
  const report = await first(db, `SELECT ${REPORT_COLUMNS} FROM reports r ${ISSUE_JOIN} WHERE r.report_id = ?`, reportId);
  if (!report) return null;
  const [issues, prs] = await Promise.all([
    all(
      db,
      `SELECT i.*, ri.source AS source FROM report_issue ri JOIN issues i ON i.number = ri.issue_number
       WHERE ri.report_id = ? AND ${LINKED} ORDER BY (COALESCE(i.kind, '') = 'fallback'), i.number ASC`,
      reportId,
    ),
    all(
      db,
      `SELECT p.* FROM report_issue ri JOIN issue_pr p ON p.issue_number = ri.issue_number
       WHERE ri.report_id = ? AND ${LINKED} ORDER BY p.pr_number DESC`,
      reportId,
    ),
  ]);
  return { report, issues, prs };
}

/**
 * Daily counts of one signature since a time.
 *
 * @param {D1Database} db Database.
 * @param {string} signature Signature.
 * @param {number} since Range start, in milliseconds.
 * @returns {Promise<Map<number, number>>} Day index → count.
 */
export async function signatureDays(db, signature, since) {
  const rows = await all(
    db,
    `SELECT CAST(received_at / ? AS INTEGER) AS day, COUNT(*) AS n FROM reports
     WHERE signature = ? AND received_at >= ? GROUP BY day`,
    DAY_MS,
    signature,
    since,
  );
  return new Map(rows.map((r) => [Number(r.day), Number(r.n)]));
}

/**
 * Whether a report's version is the fixed version or a later one, so a
 * report of a fixed issue's signature from it is a regression. Build
 * metadata and distribution suffixes on the fixed version count as it;
 * a version that does not parse never counts.
 *
 * @param {string} version A report's version.
 * @param {string} fixedIn The fixed-in version (no leading `v`).
 * @returns {boolean} True for a regression.
 */
export function isRegressionVersion(version, fixedIn) {
  if (isPackagingDerivative(normalizeVersion(version), normalizeVersion(fixedIn))) return true;
  const cmp = compareSemver(version, fixedIn);
  return cmp !== null && cmp >= 0;
}

/**
 * Common table expression `latest` (`issue_number`, `source`, `fixed_in`,
 * `pr_number`, `recorded_at`, `recorded_by`, `checked_until`, `checked_tag`):
 * each issue's
 * newest fixed-in row.
 */
export const LATEST_FIX = `latest AS (
  SELECT f.issue_number, f.source, f.fixed_in, f.pr_number, f.recorded_at, f.recorded_by, f.checked_until, f.checked_tag FROM issue_fix f
  WHERE f.recorded_at = (SELECT MAX(g.recorded_at) FROM issue_fix g WHERE g.issue_number = f.issue_number))`;

/**
 * Issues tracking at least one report with a signature.
 *
 * @param {D1Database} db Database.
 * @param {string} signature Signature.
 * @returns {Promise<number[]>} Issue numbers, ascending.
 */
export async function issuesTrackingSignature(db, signature) {
  const rows = await all(
    db,
    `SELECT DISTINCT ri.issue_number AS n FROM report_issue ri JOIN reports r ON r.report_id = ri.report_id
     WHERE r.signature = ? AND ${LINKED} ORDER BY ri.issue_number`,
    signature,
  );
  return rows.map((r) => Number(r.n));
}

/**
 * Per issue with a fix: which version fixed it (the latest `issue_fix` row),
 * when its newest merged pull request landed, and how many reports sharing
 * a signature with the issue's tracked reports came from the fixed version
 * or a later one (`after`, a regression count) or, while the fix is
 * unreleased or its version does not parse, arrived after the merge.
 *
 * Counts are aggregated per issue and version in SQL; only the distinct
 * versions reach the Worker.
 *
 * @param {D1Database} db Database.
 * @returns {Promise<Map<number, {fixedIn: string | null, source: string | null,
 *   prNumber: number | null, fixAt: number | null, after: number}>>} Issues
 *   with a fixed-in row or a merged pull request. `source` is the row's
 *   (`release`, `manual`, `unreleased`) or null for a merged pull request
 *   the sync has not resolved yet.
 */
export async function fixStatus(db) {
  const FIXED = `WITH ${LATEST_FIX},
     merged AS (
       SELECT issue_number, MAX(merged_at) AS fix_at FROM issue_pr
       WHERE state = 'merged' AND merged_at IS NOT NULL GROUP BY issue_number),
     fixed AS (
       SELECT i.number AS issue_number, l.source AS source, l.fixed_in AS fixed_in, l.pr_number AS pr_number, m.fix_at AS fix_at
       FROM issues i LEFT JOIN latest l ON l.issue_number = i.number LEFT JOIN merged m ON m.issue_number = i.number
       WHERE l.issue_number IS NOT NULL OR m.issue_number IS NOT NULL)`;
  const fixes = await all(db, `${FIXED} SELECT * FROM fixed`);
  const out = new Map();
  const opt = (v) => (v === null || v === undefined ? null : v);
  for (const f of fixes) {
    out.set(Number(f.issue_number), {
      fixedIn: opt(f.fixed_in) === null ? null : String(f.fixed_in),
      source: opt(f.source) === null ? null : String(f.source),
      prNumber: opt(f.pr_number) === null ? null : Number(f.pr_number),
      fixAt: opt(f.fix_at) === null ? null : Number(f.fix_at),
      after: 0,
    });
  }
  if (!out.size) return out;
  // Reports sharing a signature with each fixed issue's tracked reports,
  // per version (a report counts once per issue even when it shares two
  // signatures with it).
  const counts = await all(
    db,
    `${FIXED},
     sigs AS (
       SELECT DISTINCT ri.issue_number, r.signature FROM report_issue ri
       JOIN fixed f ON f.issue_number = ri.issue_number
       JOIN reports r ON r.report_id = ri.report_id WHERE r.signature IS NOT NULL AND ${LINKED})
     SELECT s.issue_number AS issue_number, r2.version AS version, COUNT(DISTINCT r2.report_id) AS n,
       COUNT(DISTINCT CASE WHEN f.fix_at IS NOT NULL AND r2.received_at > f.fix_at THEN r2.report_id END) AS n_after
     FROM sigs s JOIN fixed f ON f.issue_number = s.issue_number JOIN reports r2 ON r2.signature = s.signature
     GROUP BY s.issue_number, r2.version`,
  );
  for (const c of counts) {
    const fix = out.get(Number(c.issue_number));
    if (!fix) continue;
    const byVersion = fix.fixedIn !== null && compareSemver(fix.fixedIn, fix.fixedIn) !== null;
    fix.after += byVersion ? (isRegressionVersion(String(c.version ?? ""), fix.fixedIn) ? Number(c.n) : 0) : Number(c.n_after);
  }
  return out;
}

/**
 * Common table expression `tr` (`report_id`, `issue_number`): each linked
 * report with the issue that tracks it, by `TRACKER_ORDER`, computed once
 * per statement. A report linked to two issues appears once, so issue
 * counts add up to the number of reports.
 */
const WITH_TRACKED = `WITH tr AS (
  SELECT report_id, issue_number FROM (
    SELECT ri.report_id AS report_id, ri.issue_number AS issue_number,
      ROW_NUMBER() OVER (PARTITION BY ri.report_id ORDER BY ${TRACKER_ORDER}) AS rn
    FROM report_issue ri LEFT JOIN issues ti ON ti.number = ri.issue_number WHERE ${LINKED})
  WHERE rn = 1)`;

/**
 * `WITH_TRACKED` limited to the reports one issue has a ledger row for (the
 * first bound parameter), so a page about one issue ranks only those
 * reports' links. The result is the same for that issue as the full table's,
 * since a report's tracker depends only on its own rows.
 */
const WITH_TRACKED_FOR = `WITH tr AS (
  SELECT report_id, issue_number FROM (
    SELECT ri.report_id AS report_id, ri.issue_number AS issue_number,
      ROW_NUMBER() OVER (PARTITION BY ri.report_id ORDER BY ${TRACKER_ORDER}) AS rn
    FROM report_issue ri LEFT JOIN issues ti ON ti.number = ri.issue_number
    WHERE ${LINKED} AND ri.report_id IN (SELECT x.report_id FROM report_issue x WHERE x.issue_number = ?))
  WHERE rn = 1)`;

/**
 * Diagnostics issues with the counts of the reports they track, first and
 * last receipt, daily activity in a range and linked pull requests.
 *
 * @param {D1Database} db Database.
 * @param {"open" | "closed" | "all"} state Which issues.
 * @param {number} since Activity range start, in milliseconds.
 * @param {number} limit Most issues, newest activity first.
 * @returns {Promise<object[]>} Issue rows with `reports`, `first_seen`,
 *   `last_seen`, `first_version`, `last_version`, `days` (Map) and `prs`.
 */
export async function listIssues(db, state, since, limit) {
  const filter = state === "all" ? "" : "WHERE i.state = ?";
  const params = state === "all" ? [] : [state];
  const issues = await all(
    db,
    `${WITH_TRACKED}
     SELECT i.*, COUNT(r.report_id) AS reports, MIN(r.received_at) AS first_seen,
       MAX(r.received_at) AS last_seen,
       (SELECT r2.version FROM tr t2 JOIN reports r2 ON r2.report_id = t2.report_id
          WHERE t2.issue_number = i.number ORDER BY r2.received_at ASC LIMIT 1) AS first_version,
       (SELECT r3.version FROM tr t3 JOIN reports r3 ON r3.report_id = t3.report_id
          WHERE t3.issue_number = i.number ORDER BY r3.received_at DESC LIMIT 1) AS last_version
     FROM issues i
     LEFT JOIN tr ON tr.issue_number = i.number
     LEFT JOIN reports r ON r.report_id = tr.report_id
     ${filter}
     GROUP BY i.number ORDER BY COALESCE(MAX(r.received_at), i.updated_at) DESC LIMIT ?`,
    ...params,
    limit,
  );
  const [days, prs] = await Promise.all([
    all(
      db,
      `${WITH_TRACKED}
       SELECT tr.issue_number AS issue, CAST(r.received_at / ? AS INTEGER) AS day, COUNT(*) AS n
       FROM tr JOIN reports r ON r.report_id = tr.report_id
       WHERE r.received_at >= ? GROUP BY tr.issue_number, day`,
      DAY_MS,
      since,
    ),
    all(db, "SELECT * FROM issue_pr ORDER BY pr_number DESC"),
  ]);
  return issues.map((i) => ({
    ...i,
    days: new Map(days.filter((d) => d.issue === i.number).map((d) => [Number(d.day), Number(d.n)])),
    prs: prs.filter((p) => p.issue_number === i.number),
  }));
}

/**
 * Newest access-log rows, for owners.
 *
 * @param {D1Database} db Database.
 * @param {number} limit Most rows.
 * @returns {Promise<object[]>} Rows, newest first.
 */
export async function recentAccess(db, limit) {
  return all(db, "SELECT at, login, action, report_id FROM access_log ORDER BY at DESC, id DESC LIMIT ?", limit);
}

/**
 * Appends an access-log row.
 *
 * @param {D1Database} db Database.
 * @param {{userId: number, login: string}} who Signed-in member.
 * @param {"sign-in" | "view-report" | "sign-out-all" | "reindex" | "sync" | "attach" | "detach" | "move"
 *   | "merge" | "update-issue"} action What they did.
 * @param {string | null} reportId Report viewed, when any.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<void>}
 */
export async function logAccess(db, who, action, reportId, now) {
  await run(
    db,
    "INSERT INTO access_log (at, user_id, login, action, report_id) VALUES (?, ?, ?, ?, ?)",
    now,
    who.userId,
    who.login,
    action,
    reportId,
  );
}

/** Most issues `issueSummaries` hands the ingest Action per call. */
export const SUMMARY_BATCH = 20;

/** Most versions and platforms a summary lists; the rest are "others". */
const SUMMARY_TOP = 8;

/** Most newest reports a summary lists. */
const SUMMARY_NEWEST = 10;

/**
 * The summary the ingest Action keeps as one comment on each GitHub issue,
 * for every issue whose current summary differs from the one last written
 * (`issues.summary_sent`): how many reports the issue tracks and from how
 * many installations, how they were linked, first and last receipt, the
 * versions and platforms seen, and the newest reports. The counts come from
 * the ledger (`report_issue`, each report counted under the issue that
 * tracks it), so a member's attach or detach on the dashboard changes the
 * fingerprint and reaches GitHub on the next run. The holding issue for
 * untriaged reports is summarized like any other (its body lists no
 * visible IDs, so the comment is what people read). An issue with no
 * tracked report and no summary written is left out; one whose last report
 * was detached gets an empty summary once.
 *
 * @param {D1Database} db Database.
 * @param {number} limit Most issues to return, most recently active first.
 * @param {number | null} [only] One issue number to answer for (its page
 *   asks whether its comment is out of date), or null for every issue.
 * @returns {Promise<Array<{issue: number, fingerprint: string,
 *   reports: number, installs: number, first_seen: number | null,
 *   last_seen: number | null, sources: {filed: number, signature: number,
 *   triage: number, manual: number}, versions: Array<{version: string, n: number}>,
 *   platforms: Array<{platform: string, n: number}>,
 *   newest: Array<{report_id: string, version: string, received_at: number}>}>>}
 *   Summaries to render; `fingerprint` is echoed back through
 *   `markSummarySent` once the comment is written.
 */
export async function issueSummaries(db, limit, only = null) {
  const tracked = only === null ? WITH_TRACKED : WITH_TRACKED_FOR;
  const scope = only === null ? [] : [only];
  const [totals, versions, platforms, newest] = await Promise.all([
    all(
      db,
      `${tracked}
       SELECT i.number AS issue, i.summary_sent AS sent,
         COUNT(r.report_id) AS reports, COUNT(DISTINCT NULLIF(r.install_hash, '')) AS installs,
         MIN(r.received_at) AS first_seen, MAX(r.received_at) AS last_seen,
         COALESCE(SUM(ri.source = 'filed'), 0) AS filed, COALESCE(SUM(ri.source = 'signature'), 0) AS by_signature,
         COALESCE(SUM(ri.source = 'triage'), 0) AS by_triage, COALESCE(SUM(ri.source = 'manual'), 0) AS by_manual
       FROM issues i
       LEFT JOIN tr ON tr.issue_number = i.number
       LEFT JOIN reports r ON r.report_id = tr.report_id
       LEFT JOIN report_issue ri ON ri.report_id = tr.report_id AND ri.issue_number = tr.issue_number
       ${only === null ? "" : "WHERE i.number = ?"}
       GROUP BY i.number
       HAVING COUNT(r.report_id) > 0 OR i.summary_sent IS NOT NULL`,
      ...scope,
      ...scope,
    ),
    all(
      db,
      `${tracked}
       SELECT tr.issue_number AS issue, r.version AS version, COUNT(*) AS n
       FROM tr JOIN reports r ON r.report_id = tr.report_id
       GROUP BY tr.issue_number, r.version ORDER BY tr.issue_number, n DESC, r.version`,
      ...scope,
    ),
    all(
      db,
      `${tracked}
       SELECT tr.issue_number AS issue, TRIM(r.os || ' ' || r.arch) AS platform, COUNT(*) AS n
       FROM tr JOIN reports r ON r.report_id = tr.report_id
       GROUP BY tr.issue_number, platform ORDER BY tr.issue_number, n DESC, platform`,
      ...scope,
    ),
    all(
      db,
      `${tracked}
       SELECT issue, report_id, version, received_at FROM (
         SELECT tr.issue_number AS issue, r.report_id AS report_id, r.version AS version, r.received_at AS received_at,
           ROW_NUMBER() OVER (PARTITION BY tr.issue_number ORDER BY r.received_at DESC, r.report_id) AS rn
         FROM tr JOIN reports r ON r.report_id = tr.report_id)
       WHERE rn <= ? ORDER BY issue, received_at DESC, report_id`,
      ...scope,
      SUMMARY_NEWEST,
    ),
  ]);
  const by = (rows, key) => {
    const m = new Map();
    for (const row of rows) {
      const issue = Number(row.issue);
      if (!m.has(issue)) m.set(issue, []);
      m.get(issue).push(key(row));
    }
    return m;
  };
  const top = (list, key) => {
    if (!list || list.length <= SUMMARY_TOP) return list ?? [];
    const rest = list.slice(SUMMARY_TOP).reduce((sum, e) => sum + e.n, 0);
    return [...list.slice(0, SUMMARY_TOP), { [key]: "others", n: rest }];
  };
  const v = by(versions, (r) => ({ version: String(r.version ?? ""), n: Number(r.n) }));
  const pl = by(platforms, (r) => ({ platform: String(r.platform ?? ""), n: Number(r.n) }));
  const nw = by(newest, (r) => ({ report_id: String(r.report_id), version: String(r.version ?? ""), received_at: Number(r.received_at) }));
  const out = [];
  for (const t of totals) {
    const issue = Number(t.issue);
    // Only what the comment shows is hashed: a close or reopen alone must
    // not schedule a no-op edit.
    const summary = {
      issue,
      reports: Number(t.reports),
      installs: Number(t.installs),
      first_seen: t.first_seen === null ? null : Number(t.first_seen),
      last_seen: t.last_seen === null ? null : Number(t.last_seen),
      sources: { filed: Number(t.filed), signature: Number(t.by_signature), triage: Number(t.by_triage), manual: Number(t.by_manual) },
      versions: top(v.get(issue), "version"),
      platforms: top(pl.get(issue), "platform"),
      newest: nw.get(issue) ?? [],
    };
    const fingerprint = await sha256Hex(JSON.stringify(summary));
    if (fingerprint === t.sent) continue;
    out.push({ ...summary, fingerprint });
  }
  out.sort((a, b) => (b.last_seen ?? 0) - (a.last_seen ?? 0) || b.issue - a.issue);
  return out.slice(0, limit);
}

/**
 * Records the summary the ingest Action (or the owner's "Update GitHub now"
 * button) wrote on an issue.
 *
 * @param {D1Database} db Database.
 * @param {number} issue Issue number.
 * @param {string} fingerprint The `issueSummaries` fingerprint it rendered.
 * @param {number} now Unix time in milliseconds, kept as when it was written.
 * @returns {Promise<boolean>} Whether the issue exists.
 */
export async function markSummarySent(db, issue, fingerprint, now) {
  const res = await db
    .prepare("UPDATE issues SET summary_sent = ?, summary_sent_at = ? WHERE number = ?")
    .bind(fingerprint, now, issue)
    .run();
  return Number(res?.meta?.changes ?? 0) > 0;
}

/**
 * How long a summary handed to the ingest run blocks the owner's "Update
 * GitHub now" press, in milliseconds, when the run never acknowledges it
 * (a run that failed or was cancelled).
 */
export const SUMMARY_HOLD_MS = 10 * 60_000;

/**
 * How long one "Update GitHub now" press holds an issue, in milliseconds,
 * should the Worker die before releasing it.
 */
const SUMMARY_LOCK_MS = 2 * 60_000;

/**
 * Hands summaries to the ingest run: leaves out the issues an owner's press
 * holds at the moment, and stamps the rest as handed out so a press waits
 * for the run's acknowledgement (`markSummarySent`) or `SUMMARY_HOLD_MS`.
 *
 * @template {{issue: number}} T
 * @param {D1Database} db Database.
 * @param {T[]} summaries From `issueSummaries`.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<T[]>} The summaries to render.
 */
export async function handOutSummaries(db, summaries, now) {
  if (summaries.length === 0) return summaries;
  const locked = new Set((await all(db, "SELECT number FROM issues WHERE summary_lock_until > ?", now)).map((r) => Number(r.number)));
  const out = summaries.filter((s) => !locked.has(s.issue));
  if (out.length) {
    await run(db, `UPDATE issues SET summary_handed_at = ? WHERE number IN (${out.map(() => "?").join(", ")})`, now, ...out.map((s) => s.issue));
  }
  return out;
}

/**
 * Takes an issue for one "Update GitHub now" press, in one statement: fails
 * when another press holds it, or the ingest run was handed its summary
 * within `SUMMARY_HOLD_MS` and has not acknowledged it yet.
 *
 * @param {D1Database} db Database.
 * @param {number} issue Issue number.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<boolean>} Whether the press holds the issue; release it
 *   with `releaseSummaryWrite`.
 */
export async function claimSummaryWrite(db, issue, now) {
  const res = await db
    .prepare(
      `UPDATE issues SET summary_lock_until = ?
       WHERE number = ? AND (summary_lock_until IS NULL OR summary_lock_until <= ?)
         AND (summary_handed_at IS NULL OR summary_handed_at <= COALESCE(summary_sent_at, 0) OR summary_handed_at <= ?)`,
    )
    .bind(now + SUMMARY_LOCK_MS, issue, now, now - SUMMARY_HOLD_MS)
    .run();
  return Number(res?.meta?.changes ?? 0) > 0;
}

/**
 * Releases an issue a press held (`claimSummaryWrite`).
 *
 * @param {D1Database} db Database.
 * @param {number} issue Issue number.
 * @returns {Promise<void>}
 */
export async function releaseSummaryWrite(db, issue) {
  await run(db, "UPDATE issues SET summary_lock_until = NULL WHERE number = ?", issue);
}

/** Most versions or platforms the issue page breaks down. */
const DETAIL_TOP = 8;

/** Most linked reports the issue page lists per section. */
export const DETAIL_LINKS = 200;

/** Most suggested reports the issue page lists. */
export const DETAIL_SUGGESTIONS = 50;

/** Most issues offered in the move and merge pickers. */
const PICKER_ISSUES = 100;

/**
 * One issue as its dashboard page shows it: the stored issue with its pull
 * requests, the counts of the reports it tracks (ledger rule, as the board),
 * the versions and platforms among them, the ledger rows that name the
 * issue (so a member can detach or move a report), whether the issue's
 * summary comment on GitHub is out of date (`issueSummaries` would hand the
 * issue out again) and how many ledger rows were written since it was last
 * recorded, and the reports that share a tracked signature without being
 * linked to any issue (suggestions).
 *
 * @param {D1Database} db Database.
 * @param {number} number Issue number.
 * @param {number} since Activity range start for `days`, in milliseconds.
 * @returns {Promise<{issue: object, prs: object[], reports: number,
 *   installs: number, first_seen: number | null, last_seen: number | null,
 *   versions: Array<{key: string, n: number}>, platforms: Array<{key: string, n: number}>,
 *   days: Map<number, number>, links: object[], suggested: object[],
 *   outdated: boolean, pending: number, summarySentAt: number | null} | null>}
 *   The detail, or null when the issue is not stored.
 */
export async function issueDetail(db, number, since) {
  const issue = await first(db, "SELECT * FROM issues WHERE number = ?", number);
  if (!issue) return null;
  const [prs, totals, versions, platforms, days, links, suggested, pending, summaries] = await Promise.all([
    all(db, "SELECT * FROM issue_pr WHERE issue_number = ? ORDER BY pr_number DESC", number),
    first(
      db,
      `${WITH_TRACKED_FOR}
       SELECT COUNT(r.report_id) AS reports, COUNT(DISTINCT NULLIF(r.install_hash, '')) AS installs,
         MIN(r.received_at) AS first_seen, MAX(r.received_at) AS last_seen
       FROM tr JOIN reports r ON r.report_id = tr.report_id WHERE tr.issue_number = ?`,
      number,
      number,
    ),
    all(
      db,
      `${WITH_TRACKED_FOR}
       SELECT r.version AS key, COUNT(*) AS n FROM tr JOIN reports r ON r.report_id = tr.report_id
       WHERE tr.issue_number = ? GROUP BY r.version ORDER BY n DESC, r.version LIMIT ?`,
      number,
      number,
      DETAIL_TOP,
    ),
    all(
      db,
      `${WITH_TRACKED_FOR}
       SELECT TRIM(r.os || ' ' || r.arch) AS key, COUNT(*) AS n FROM tr JOIN reports r ON r.report_id = tr.report_id
       WHERE tr.issue_number = ? GROUP BY key ORDER BY n DESC, key LIMIT ?`,
      number,
      number,
      DETAIL_TOP,
    ),
    all(
      db,
      `${WITH_TRACKED_FOR}
       SELECT CAST(r.received_at / ? AS INTEGER) AS day, COUNT(*) AS n FROM tr JOIN reports r ON r.report_id = tr.report_id
       WHERE tr.issue_number = ? AND r.received_at >= ? GROUP BY day`,
      number,
      DAY_MS,
      number,
      since,
    ),
    // Every ledger row naming the issue, with whether this issue is the one
    // the report counts under (another, older issue may track it instead).
    all(
      db,
      `${WITH_TRACKED_FOR}
       SELECT r.report_id, r.received_at, r.version, r.os, r.arch, r.signature, ri.source, ri.linked_by, ri.linked_at,
         (tr.issue_number IS NOT NULL) AS tracked
       FROM report_issue ri JOIN reports r ON r.report_id = ri.report_id
       LEFT JOIN tr ON tr.report_id = ri.report_id AND tr.issue_number = ri.issue_number
       WHERE ri.issue_number = ? ORDER BY r.received_at DESC, r.report_id LIMIT ?`,
      number,
      number,
      DETAIL_LINKS,
    ),
    all(
      db,
      `${WITH_TRACKED_FOR}
       SELECT r.report_id, r.received_at, r.version, r.os, r.arch FROM reports r
       WHERE r.signature IS NOT NULL
         AND r.signature IN (SELECT t.signature FROM tr JOIN reports t ON t.report_id = tr.report_id WHERE tr.issue_number = ?)
         AND NOT EXISTS (SELECT 1 FROM report_issue x WHERE x.report_id = r.report_id AND (x.source <> 'excluded' OR x.issue_number = ?))
       ORDER BY r.received_at DESC, r.report_id LIMIT ?`,
      number,
      number,
      number,
      DETAIL_SUGGESTIONS,
    ),
    first(
      db,
      `SELECT COUNT(*) AS n FROM report_issue ri WHERE ri.issue_number = ? AND ri.linked_at IS NOT NULL
         AND ri.linked_at > COALESCE((SELECT summary_sent_at FROM issues WHERE number = ?), 0)`,
      number,
      number,
    ),
    issueSummaries(db, 1, number),
  ]);
  return {
    issue,
    prs,
    reports: Number(totals?.reports ?? 0),
    installs: Number(totals?.installs ?? 0),
    first_seen: totals?.first_seen == null ? null : Number(totals.first_seen),
    last_seen: totals?.last_seen == null ? null : Number(totals.last_seen),
    versions: versions.map((v) => ({ key: String(v.key ?? ""), n: Number(v.n) })),
    platforms: platforms.map((v) => ({ key: String(v.key ?? ""), n: Number(v.n) })),
    days: new Map(days.map((d) => [Number(d.day), Number(d.n)])),
    links,
    suggested,
    outdated: summaries.length > 0,
    pending: Number(pending?.n ?? 0),
    summarySentAt: issue.summary_sent_at == null ? null : Number(issue.summary_sent_at),
  };
}

/**
 * Open triaged issues for the move, merge and attach pickers.
 *
 * @param {D1Database} db Database.
 * @returns {Promise<Array<{number: number, title: string}>>} Newest first.
 */
export async function openIssueOptions(db) {
  const rows = await all(
    db,
    "SELECT number, title FROM issues WHERE state = 'open' AND kind <> 'fallback' ORDER BY number DESC LIMIT ?",
    PICKER_ISSUES,
  );
  return rows.map((r) => ({ number: Number(r.number), title: String(r.title) }));
}

/**
 * The statement that writes one ledger row by a member's hand: inserted
 * when the pair has none, else updated only where `guard` holds on the
 * existing row.
 *
 * @param {D1Database} db Database.
 * @param {"manual" | "excluded"} source The row's source.
 * @param {string} guard SQL condition on the existing `report_issue` row.
 * @param {number} issue Issue number.
 * @param {string} reportId Report UUID.
 * @param {string} login Member making the change.
 * @param {number} now Unix time in milliseconds.
 * @returns {D1PreparedStatement} The bound statement.
 */
function ledgerUpsert(db, source, guard, issue, reportId, login, now) {
  return db
    .prepare(
      `INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at) VALUES (?, ?, ?, ?, ?)
       ON CONFLICT (report_id, issue_number) DO UPDATE SET source = excluded.source, linked_by = excluded.linked_by, linked_at = excluded.linked_at
       WHERE ${guard}`,
    )
    .bind(reportId, issue, source, login, now);
}

/**
 * The statement that links a report to an issue by a member's hand (source
 * `manual`). A link made another way stays as it is; a member's earlier
 * exclusion is replaced, since the member now asks for the link.
 *
 * @param {D1Database} db Database.
 * @param {number} issue Issue number.
 * @param {string} reportId Report UUID.
 * @param {string} login Member making the change.
 * @param {number} now Unix time in milliseconds.
 * @returns {D1PreparedStatement} The bound statement.
 */
function attachStatement(db, issue, reportId, login, now) {
  return ledgerUpsert(db, "manual", "report_issue.source = 'excluded'", issue, reportId, login, now);
}

/**
 * The statement that detaches a report from an issue (source `excluded`):
 * the link, however it was made, is replaced, and the sync, signature
 * matching and triage leave the pair alone from then on. A report never
 * linked to the issue (a dismissed suggestion) gets the exclusion too.
 *
 * @param {D1Database} db Database.
 * @param {number} issue Issue number.
 * @param {string} reportId Report UUID.
 * @param {string} login Member making the change.
 * @param {number} now Unix time in milliseconds.
 * @returns {D1PreparedStatement} The bound statement.
 */
function detachStatement(db, issue, reportId, login, now) {
  return ledgerUpsert(db, "excluded", "report_issue.source <> 'excluded'", issue, reportId, login, now);
}

/**
 * Applies a member's correction to the ledger (`parseLinkAction`): `attach`
 * and `detach` write one row, `move` detaches from one issue and attaches to
 * the other, and `merge` attaches every report linked to the issue to the
 * other one (by the member's hand, so it also replaces an exclusion there)
 * and excludes them all from the first, in one batch. The caller has checked
 * that the issues and the report exist.
 *
 * @param {D1Database} db Database.
 * @param {import("./links.js").LinkAction} change The correction.
 * @param {string} login Member making the change.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<void>}
 */
export async function applyLinkAction(db, change, login, now) {
  const { action, issue, report, to } = change;
  if (action === "attach" && report !== null) {
    await attachStatement(db, issue, report, login, now).run();
  } else if (action === "detach" && report !== null) {
    await detachStatement(db, issue, report, login, now).run();
  } else if (action === "move" && report !== null && to !== null) {
    await db.batch([detachStatement(db, issue, report, login, now), attachStatement(db, to, report, login, now)]);
  } else if (action === "merge" && to !== null) {
    await db.batch([
      db
        .prepare(
          `INSERT INTO report_issue (report_id, issue_number, source, linked_by, linked_at)
           SELECT ri.report_id, ?, 'manual', ?, ? FROM report_issue ri WHERE ri.issue_number = ? AND ${LINKED}
           ON CONFLICT (report_id, issue_number) DO UPDATE SET source = 'manual', linked_by = excluded.linked_by, linked_at = excluded.linked_at
           WHERE report_issue.source = 'excluded'`,
        )
        .bind(to, login, now, issue),
      db
        .prepare(
          `UPDATE report_issue SET source = 'excluded', linked_by = ?, linked_at = ? WHERE issue_number = ? AND source <> 'excluded'`,
        )
        .bind(login, now, issue),
    ]);
  }
}


/**
 * A fingerprint of everything the issue sync writes that pages show: issue
 * count, numbers, states, titles and update times, report links, and pull
 * request states, titles and merge times. Two equal fingerprints mean a
 * sync changed nothing visible, so open pages need no live refresh. Issues
 * and pull requests are listed in full (the tables hold diagnostics issues
 * only); report links are counted and summed, where a collision would only
 * skip one refresh.
 *
 * @param {D1Database} db Database.
 * @returns {Promise<string>} Opaque text to compare.
 */
export async function linkFingerprint(db) {
  const row = await first(
    db,
    `SELECT
       (SELECT COALESCE(GROUP_CONCAT(v, char(31)), '') FROM
         (SELECT number || ':' || state || ':' || updated_at || ':' || title AS v FROM issues ORDER BY number))
       || char(30) || (SELECT COUNT(*) || ':' || COALESCE(SUM(issue_number), 0) FROM report_issue)
       || char(30) || (SELECT COUNT(*) || ':' || COALESCE(MAX(recorded_at), 0) FROM issue_fix)
       || char(30) || (SELECT COALESCE(GROUP_CONCAT(v, char(31)), '') FROM
         (SELECT issue_number || ':' || pr_number || ':' || state || ':' || COALESCE(merged_at, '') || ':' || title AS v FROM issue_pr ORDER BY issue_number, pr_number))
       AS fp`,
  );
  return row ? String(row.fp) : "";
}

/**
 * Reads a cursor.
 *
 * @param {D1Database} db Database.
 * @param {string} key Cursor name.
 * @returns {Promise<string | null>} Its value.
 */
export async function getState(db, key) {
  const row = await first(db, "SELECT value FROM sync_state WHERE key = ?", key);
  return row ? String(row.value) : null;
}

/**
 * Writes a cursor.
 *
 * @param {D1Database} db Database.
 * @param {string} key Cursor name.
 * @param {string} value New value.
 * @returns {Promise<void>}
 */
export async function setState(db, key, value) {
  await stateStatement(db, key, value).run();
}

/**
 * The statement that writes a cursor, for saving it in the same batch as the
 * work it records.
 *
 * @param {D1Database} db Database.
 * @param {string} key Cursor name.
 * @param {string | null} value New value; null deletes the cursor.
 * @returns {D1PreparedStatement} The bound statement.
 */
export function stateStatement(db, key, value) {
  if (value === null) return db.prepare("DELETE FROM sync_state WHERE key = ?").bind(key);
  return db
    .prepare("INSERT INTO sync_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
    .bind(key, value);
}

/**
 * Reads several cursors in one query.
 *
 * @param {D1Database} db Database.
 * @param {string[]} keys Cursor names.
 * @returns {Promise<Map<string, string>>} Name → value, for the cursors set.
 */
export async function getStates(db, keys) {
  const rows = await all(db, `SELECT key, value FROM sync_state WHERE key IN (${keys.map(() => "?").join(", ")})`, ...keys);
  return new Map(rows.map((r) => [String(r.key), String(r.value)]));
}

/**
 * Deletes expired sessions and access-log rows older than 90 days.
 *
 * @param {D1Database} db Database.
 * @param {number} now Unix time in milliseconds.
 * @returns {Promise<void>}
 */
export async function sweep(db, now) {
  await run(db, "DELETE FROM sessions WHERE expires_at <= ?", now);
  await run(db, "DELETE FROM access_log WHERE at < ?", now - 90 * DAY_MS);
}
