-- The fingerprint of the summary comment the ingest Action last rendered on
-- each GitHub issue (GET /issues/summaries hands the Action every issue whose
-- current summary differs; POST /issues/summary records the one it wrote).
-- NULL means no summary comment has been written yet.
ALTER TABLE issues ADD COLUMN summary_sent TEXT;
