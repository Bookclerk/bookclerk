-- How long each session took to start, so the launch and waiting pages can
-- say how long a start usually takes (tools/demo/src/limits.ts).

-- Seconds from the visitor pressing Start to their copy answering as ready.
-- NULL while the session starts, and for sessions that never became ready.
ALTER TABLE sessions ADD COLUMN startup_seconds INTEGER;
