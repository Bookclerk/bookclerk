-- Which version fixed each diagnostics issue, so a report of the same
-- signature from that version or a later one is a regression rather than
-- an expected report from an unfixed install.
--
-- One row per change, newest wins (the latest recorded_at):
-- source: 'release'    the first release tag that contains the fix pull
--                      request's merge commit, found by the sync
--         'unreleased' the fix merged but no release contains it yet; the
--                      sync looks again when a newer release appears
--         'manual'     a member set it, with a `fixed-in:<version>` label on
--                      the issue (or, later, on the dashboard)
-- fixed_in: the version without a leading `v`; NULL while unreleased.
-- pr_number / merge_sha: the fix pull request the row was derived from.
-- checked_until / checked_tag: for an unreleased row, the publish time
--                (Unix ms) and tag of the last release compared, so the next
--                look continues with the releases after it (releases are
--                ordered by publish time, then tag, so two published in the
--                same second are told apart by tag).
-- recorded_by: 'sync', 'label' (a fixed-in label) or a member login.
CREATE TABLE issue_fix (
  issue_number INTEGER NOT NULL,
  recorded_at INTEGER NOT NULL,
  source TEXT NOT NULL,
  fixed_in TEXT,
  pr_number INTEGER,
  merge_sha TEXT,
  checked_until INTEGER,
  checked_tag TEXT,
  recorded_by TEXT NOT NULL,
  PRIMARY KEY (issue_number, recorded_at)
);

-- The merge commit of each merged pull request (40 hex characters), so the
-- sync can ask which release tag contains it.
ALTER TABLE issue_pr ADD COLUMN merge_sha TEXT;

-- The version named by the issue's `fixed-in:<version>` label, if any,
-- copied by the sync; it overrides the automatic fixed-in version.
ALTER TABLE issues ADD COLUMN fixed_in_label TEXT;
