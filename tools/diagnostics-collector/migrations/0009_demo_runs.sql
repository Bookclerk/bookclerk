-- Hosted demo runs, reported live by the demo Worker over a service binding
-- (demo-runs.ts). No visitor identity: a random run ID, times in Unix
-- seconds, the image version and why the run ended.

CREATE TABLE demo_runs (
  -- The demo's random ID for the run (a version-4 UUID), not its session.
  run_id TEXT PRIMARY KEY,
  -- Start and hard stop, Unix seconds.
  started_at INTEGER NOT NULL,
  ends_at INTEGER NOT NULL,
  -- The demo image's version.
  version TEXT NOT NULL DEFAULT '',
  -- Seconds from Start to the copy answering; NULL until it does.
  startup_seconds INTEGER,
  -- When it stopped (Unix seconds) and why: the demo's reasons (visitor,
  -- idle, time, failed, stopped, sweep), or missed when no stop arrived
  -- by the hard stop plus a grace. NULL while the run is live.
  ended_at INTEGER,
  end_reason TEXT
);
-- Live runs and the missed-run sweep.
CREATE INDEX demo_runs_open ON demo_runs (ends_at) WHERE ended_at IS NULL;
-- Daily figures and retention.
CREATE INDEX demo_runs_started ON demo_runs (started_at);
