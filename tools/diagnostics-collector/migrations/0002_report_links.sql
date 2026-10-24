-- How each report came to be linked to an issue, so later links survive the
-- issue sync and every count can read one ledger.
--
-- source: 'filed'     listed in the issue body's Report IDs section (the sync
--                     replaces these rows from GitHub on every pass)
--         'signature' same error signature as a report the issue already
--                     tracks, linked by the collector at ingest
--         'triage'    the model attached it to an existing issue
--         'manual'    a member attached it on the dashboard
--         'excluded'  a member detached it from this issue; nothing links the
--                     pair again (the report may be linked to another issue)
-- linked_by: 'ingest', 'sync' or a member login; linked_at: Unix ms.
ALTER TABLE report_issue ADD COLUMN source TEXT NOT NULL DEFAULT 'filed';
ALTER TABLE report_issue ADD COLUMN linked_by TEXT;
ALTER TABLE report_issue ADD COLUMN linked_at INTEGER;
