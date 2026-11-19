-- Each session's run ID for live usage tracking (src/usage.ts): a random
-- version-4 UUID made when the session is admitted, separate from the
-- session id, so the diagnostics dashboard can match a run's start, ready
-- and stop without learning anything about the visitor. Empty for rows
-- from before this column.
ALTER TABLE sessions ADD COLUMN run_id TEXT NOT NULL DEFAULT '';
