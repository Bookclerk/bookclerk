-- When the ingest Action last wrote an issue's summary comment (Unix ms),
-- set with summary_sent, so the issue page can count the ledger rows written
-- since. Issues with a summary already recorded start from now, so their
-- older ingest links are not counted as pending.
ALTER TABLE issues ADD COLUMN summary_sent_at INTEGER;
UPDATE issues SET summary_sent_at = CAST(strftime('%s', 'now') AS INTEGER) * 1000 WHERE summary_sent IS NOT NULL;
