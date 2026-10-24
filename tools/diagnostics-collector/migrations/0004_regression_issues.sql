-- Regression issues: an issue ingest files when reports with a closed
-- issue's error signature arrive from the version that fixed it or a later
-- one. Its body starts with
-- `<!-- bookclerk-diagnostics: regression of #<n> -->`; the sync reads the
-- number into regression_of (NULL for every other issue), so the pages can
-- show which issue a regression issue goes back to.
ALTER TABLE issues ADD COLUMN regression_of INTEGER;
