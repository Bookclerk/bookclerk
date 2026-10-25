-- Hosted demo bookkeeping (tools/demo/src/limits.ts). Nothing here names a
-- visitor: sessions and rate rows key on HMACs of the session cookie and of
-- the client address salted with the UTC day, so rows cannot be linked to an
-- address or across days.

-- One row per demo session.
CREATE TABLE sessions (
  -- HMAC of the visitor's session cookie (hex); also names the container.
  id TEXT PRIMARY KEY,
  -- HMAC of the client address and the UTC day the session started (hex).
  ip_hash TEXT NOT NULL,
  -- Start, hard stop and end, in Unix seconds. ended_at is NULL while live.
  started_at INTEGER NOT NULL,
  ends_at INTEGER NOT NULL,
  ended_at INTEGER,
  -- Why it ended: visitor, idle, time, failed, stopped or sweep.
  end_reason TEXT,
  -- Store sign-in attempts the visitor started in this session.
  signins INTEGER NOT NULL DEFAULT 0,
  -- Store accounts signed out at teardown, and sign-outs that failed.
  revoked INTEGER NOT NULL DEFAULT 0,
  revoke_failed INTEGER NOT NULL DEFAULT 0,
  -- Commit the demo image was built from.
  version TEXT NOT NULL DEFAULT ''
);
CREATE INDEX sessions_live ON sessions (ended_at, ends_at);

-- Visitor-seconds reserved per UTC day ("YYYY-MM-DD"). A new session
-- reserves its full length; teardown gives back what it did not use.
CREATE TABLE budget (
  day TEXT PRIMARY KEY,
  reserved INTEGER NOT NULL
);

-- Per-address counters: kind "session" per UTC hour, "signin" per UTC day.
-- window_start is that hour or day in Unix seconds.
CREATE TABLE rate (
  ip_hash TEXT NOT NULL,
  kind TEXT NOT NULL,
  window_start INTEGER NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (ip_hash, kind, window_start)
);
