/**
 * Queries over the dashboard's D1 database (binding DASHBOARD_DB, schema in
 * migrations/). Every statement binds its values; no SQL is built from
 * request or report text, only from the fixed filter names below.
 */

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

/** Default daily cap on rows /submit may index. */
export const DEFAULT_INDEX_DAILY_ROWS = 20_000;

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
  const s = row.summary;
  const insert = await db
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
    )
    .run();
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
    clauses.push(
      `${f.filed === "no" ? "NOT " : ""}EXISTS (SELECT 1 FROM report_issue ri WHERE ri.report_id = r.report_id)`,
    );
  }
  return { where: clauses.join(" AND "), params };
}

/** Columns for a report row with its newest linked issue. */
const REPORT_COLUMNS = `r.report_id, r.object_key, r.received_at, r.version, r.os, r.arch, r.distro,
  r.trigger, r.events, r.errors, r.warnings, r.signature, r.signature_label, r.install_hash,
  i.number AS issue_number, i.state AS issue_state`;

/** Join to the newest issue that lists a report. */
const ISSUE_JOIN = `LEFT JOIN issues i ON i.number = (
  SELECT MAX(ri.issue_number) FROM report_issue ri WHERE ri.report_id = r.report_id)`;

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
    first(db, `SELECT COUNT(*) AS n FROM reports r WHERE ${range.where} AND NOT EXISTS (SELECT 1 FROM report_issue ri WHERE ri.report_id = r.report_id)`, ...range.params),
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
           WHERE r.signature = ? AND r.received_at >= ? GROUP BY day`,
          DAY_MS,
          s.signature,
          since,
        ),
        first(
          db,
          `SELECT i.number AS number, i.state AS state FROM reports r
           JOIN report_issue ri ON ri.report_id = r.report_id
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
 * One indexed report with its linked issues and their pull requests.
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
      `SELECT i.* FROM report_issue ri JOIN issues i ON i.number = ri.issue_number
       WHERE ri.report_id = ? ORDER BY i.number DESC`,
      reportId,
    ),
    all(
      db,
      `SELECT p.* FROM report_issue ri JOIN issue_pr p ON p.issue_number = ri.issue_number
       WHERE ri.report_id = ? ORDER BY p.pr_number DESC`,
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
 * Per issue: when its newest merged pull request landed and how many reports
 * sharing a signature with the issue's own reports arrived after that.
 *
 * @param {D1Database} db Database.
 * @returns {Promise<Map<number, {fixAt: number, after: number}>>} Only issues
 *   with a merged pull request.
 */
export async function afterFixCounts(db) {
  const rows = await all(
    db,
    `WITH fixes AS (
       SELECT issue_number, MAX(merged_at) AS fix_at FROM issue_pr
       WHERE state = 'merged' AND merged_at IS NOT NULL GROUP BY issue_number),
     sigs AS (
       SELECT DISTINCT ri.issue_number, r.signature FROM report_issue ri
       JOIN reports r ON r.report_id = ri.report_id WHERE r.signature IS NOT NULL)
     SELECT f.issue_number AS issue_number, f.fix_at AS fix_at,
       (SELECT COUNT(DISTINCT r2.report_id) FROM sigs s
          JOIN reports r2 ON r2.signature = s.signature AND r2.received_at > f.fix_at
          WHERE s.issue_number = f.issue_number) AS after
     FROM fixes f`,
  );
  return new Map(rows.map((r) => [Number(r.issue_number), { fixAt: Number(r.fix_at), after: Number(r.after) }]));
}

/**
 * Diagnostics issues with report counts, first and last receipt, daily
 * activity in a range and linked pull requests.
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
    `SELECT i.*, COUNT(r.report_id) AS reports, MIN(r.received_at) AS first_seen,
       MAX(r.received_at) AS last_seen,
       (SELECT r2.version FROM report_issue x JOIN reports r2 ON r2.report_id = x.report_id
          WHERE x.issue_number = i.number ORDER BY r2.received_at ASC LIMIT 1) AS first_version,
       (SELECT r3.version FROM report_issue y JOIN reports r3 ON r3.report_id = y.report_id
          WHERE y.issue_number = i.number ORDER BY r3.received_at DESC LIMIT 1) AS last_version
     FROM issues i
     LEFT JOIN report_issue ri ON ri.issue_number = i.number
     LEFT JOIN reports r ON r.report_id = ri.report_id
     ${filter}
     GROUP BY i.number ORDER BY COALESCE(MAX(r.received_at), i.updated_at) DESC LIMIT ?`,
    ...params,
    limit,
  );
  const [days, prs] = await Promise.all([
    all(
      db,
      `SELECT ri.issue_number AS issue, CAST(r.received_at / ? AS INTEGER) AS day, COUNT(*) AS n
       FROM report_issue ri JOIN reports r ON r.report_id = ri.report_id
       WHERE r.received_at >= ? GROUP BY ri.issue_number, day`,
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
 * @param {"sign-in" | "view-report" | "sign-out-all" | "reindex"} action What they did.
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
  await run(
    db,
    "INSERT INTO sync_state (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    key,
    value,
  );
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
