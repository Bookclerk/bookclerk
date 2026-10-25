-- Dashboard index, sessions and audit log (D1 binding DASHBOARD_DB).
-- B2 stays the store of record; every row here can be rebuilt from it and
-- from GitHub, except sessions and the access log.

-- One row per stored report, written by /submit after the B2 upload and by
-- the owner-only reindex. Raw event messages are not copied here; only a
-- normalized signature label (dashboard/signature.ts).
CREATE TABLE reports (
  report_id TEXT PRIMARY KEY,
  object_key TEXT NOT NULL,
  received_at INTEGER NOT NULL,
  version TEXT NOT NULL DEFAULT '',
  os TEXT NOT NULL DEFAULT '',
  arch TEXT NOT NULL DEFAULT '',
  distro TEXT NOT NULL DEFAULT '',
  trigger TEXT NOT NULL DEFAULT '',
  events INTEGER NOT NULL DEFAULT 0,
  errors INTEGER NOT NULL DEFAULT 0,
  warnings INTEGER NOT NULL DEFAULT 0,
  signature TEXT,
  signature_label TEXT,
  install_hash TEXT NOT NULL DEFAULT ''
);
CREATE INDEX reports_received ON reports (received_at);
CREATE INDEX reports_signature ON reports (signature, received_at);

-- Rows indexed per UTC day, for the daily cap that keeps an open /submit
-- from spending the D1 write allowance.
CREATE TABLE index_days (
  day TEXT PRIMARY KEY,
  rows INTEGER NOT NULL
);

-- Diagnostics issues (label `diagnostics`), synced from GitHub by the cron.
-- prs_synced_at is when its pull requests were last refreshed; NULL queues
-- the issue for the next cron run (set NULL whenever the issue changes).
CREATE TABLE issues (
  number INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  state TEXT NOT NULL,
  kind TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  closed_at INTEGER,
  prs_synced_at INTEGER
);
CREATE INDEX issues_prs_synced ON issues (prs_synced_at);

-- Which reports each issue lists in its Report IDs section.
CREATE TABLE report_issue (
  report_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  PRIMARY KEY (report_id, issue_number)
);
CREATE INDEX report_issue_issue ON report_issue (issue_number);

-- Pull requests that close or cross-reference each issue.
CREATE TABLE issue_pr (
  issue_number INTEGER NOT NULL,
  pr_number INTEGER NOT NULL,
  title TEXT NOT NULL,
  state TEXT NOT NULL,
  merged_at INTEGER,
  PRIMARY KEY (issue_number, pr_number)
);

-- Signed-in members. id_hash is SHA-256 of the cookie value; the cookie
-- itself is never stored.
CREATE TABLE sessions (
  id_hash TEXT PRIMARY KEY,
  user_id INTEGER NOT NULL,
  login TEXT NOT NULL,
  role TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
CREATE INDEX sessions_expires ON sessions (expires_at);

-- Sign-ins and report views, kept 90 days.
CREATE TABLE access_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  at INTEGER NOT NULL,
  user_id INTEGER NOT NULL,
  login TEXT NOT NULL,
  action TEXT NOT NULL,
  report_id TEXT
);
CREATE INDEX access_log_at ON access_log (at);

-- Cursors: issue sync time, reindex position.
CREATE TABLE sync_state (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
