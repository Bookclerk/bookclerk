-- When the ingest Action last wrote an issue's summary comment (Unix ms),
-- set with summary_sent, so the issue page can count the ledger rows written
-- since. Issues with a summary already recorded start from now, so their
-- older ingest links are not counted as pending.
ALTER TABLE issues ADD COLUMN summary_sent_at INTEGER;
UPDATE issues SET summary_sent_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000 WHERE summary_sent IS NOT NULL;
-- Two writers keep that comment: the ingest run and an owner's "Update GitHub
-- now" press. They take turns per issue: the run's hand-out is stamped here
-- (summary_handed_at; acknowledged once summary_sent_at passes it) and a
-- press holds the issue until it is done (summary_lock_until, Unix ms), so
-- neither writes over the other and then records the other's figures.
ALTER TABLE issues ADD COLUMN summary_handed_at INTEGER;
ALTER TABLE issues ADD COLUMN summary_lock_until INTEGER;
