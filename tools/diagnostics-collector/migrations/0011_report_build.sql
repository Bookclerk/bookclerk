-- The compiler and commit a report was built with, kept in the index.
--
-- The report page used to read both from the stored copy only, so they
-- went missing once the sender deleted it (lookup.ts). The index keeps them
-- next to version and platform, which already survive a deletion.
--
-- rustc:        `rustc_release`, then a space and `rustc_channel` when the
--               report names one; "" when it named neither.
-- build_commit: 7 to 40 lowercase hex digits, or "".
ALTER TABLE reports ADD COLUMN rustc TEXT NOT NULL DEFAULT '';
ALTER TABLE reports ADD COLUMN build_commit TEXT NOT NULL DEFAULT '';
