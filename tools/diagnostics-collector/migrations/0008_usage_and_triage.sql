-- Opt-in usage check-ins (checkin.ts) and triage state for reports.

-- One salt per UTC month. An install key is SHA-256(salt || install id);
-- the cron deletes every salt but the current month's, so keys from
-- different months cannot be joined.
CREATE TABLE install_salts (
  month TEXT PRIMARY KEY,
  salt TEXT NOT NULL
);

-- One row per install key and Unix week, kept 35 days. No install ID and no
-- address is stored.
CREATE TABLE checkins (
  week INTEGER NOT NULL,
  install_key TEXT NOT NULL,
  month TEXT NOT NULL,
  day INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  version TEXT NOT NULL,
  os TEXT NOT NULL,
  arch TEXT NOT NULL,
  kind TEXT NOT NULL,
  PRIMARY KEY (week, install_key)
);
-- Each check-in writes three rows (the row and its two indexes) plus its
-- day's counter; the counts read the day index, and the table never holds
-- more than 35 days.
CREATE INDEX checkins_day ON checkins (day);

-- Check-ins stored per UTC day (days since the epoch), for the daily cap: a
-- slot is reserved here in one statement, so the cap costs one row read
-- however many check-ins the day already holds.
CREATE TABLE checkin_days (
  day INTEGER PRIMARY KEY,
  rows INTEGER NOT NULL
);

-- Reports sent by an install that also shares usage carry its install key
-- for the month the report arrived ('' otherwise). deployment is 'demo' for
-- the hosted demo, else ''. route is the HTTP route of the triggering
-- event, when it has one; peak_latency_ms the slowest request in the report.
ALTER TABLE reports ADD COLUMN install_key TEXT NOT NULL DEFAULT '';
ALTER TABLE reports ADD COLUMN deployment TEXT NOT NULL DEFAULT '';
ALTER TABLE reports ADD COLUMN route TEXT NOT NULL DEFAULT '';
ALTER TABLE reports ADD COLUMN peak_latency_ms INTEGER;

-- Triage state: '' (needs triage), 'seen' or 'noise', who set it and when.
ALTER TABLE reports ADD COLUMN triage_state TEXT NOT NULL DEFAULT '';
ALTER TABLE reports ADD COLUMN triaged_by TEXT;
ALTER TABLE reports ADD COLUMN triaged_at INTEGER;
CREATE INDEX reports_triage ON reports (triage_state, received_at);

-- Muted signatures: reports with one leave the triage queue and the ingest
-- Action's unfiled pull until the mute ends (until, Unix ms) or a report
-- arrives from a version at or after until_version.
CREATE TABLE signature_mutes (
  signature TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  muted_by TEXT NOT NULL,
  muted_at INTEGER NOT NULL,
  until INTEGER,
  until_version TEXT
);
