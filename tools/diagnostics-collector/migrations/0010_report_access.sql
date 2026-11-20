-- Access codes for diagnostics reports (report-access.ts, lookup.ts).
--
-- /submit gives each report a random access code and answers with it; only
-- the sending install sees it. This table keeps a SHA-256 hash of
-- "<report_id>:<code>", never the code. With the code, the sender can read
-- the report back on /lookup or delete its stored copy.
--
-- object_key: the report's B2 object, so a lookup does not depend on the
--             dashboard's index row (written in the background).
-- deleted_at: when the sender deleted the stored copy (Unix ms), or NULL.
--             The dashboard's index row and issue links stay, so counts and
--             tracking are unchanged; only the report body is gone.
-- Rows are pruned 181 days after created_at, a day after B2's lifecycle
-- rule removes the report itself, by a once-a-day sweep. No index on
-- created_at and no rowid: each code then costs one written row (the
-- table's primary-key tree), and the daily sweep's
-- scan is cheap next to D1's read allowance.
CREATE TABLE report_access (
  report_id TEXT PRIMARY KEY,
  code_hash TEXT NOT NULL,
  object_key TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  deleted_at INTEGER
) WITHOUT ROWID;

-- When a report's sender deleted its stored copy, so the dashboard's report
-- page says so instead of "could not be read from storage".
ALTER TABLE reports ADD COLUMN deleted_at INTEGER;
