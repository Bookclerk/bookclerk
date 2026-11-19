-- Reports carry no install identity. install_hash was meant to hold a
-- salted hash of the client address, but the salt was never set in
-- production and the collector stopped keeping anything derived from the
-- address, so every row holds ''. The dashboard no longer reads or writes
-- the column, and its "installs" counts went with it.
ALTER TABLE reports DROP COLUMN install_hash;
