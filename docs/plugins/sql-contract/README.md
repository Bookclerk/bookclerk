# SQL contract

This folder holds the SQL that every database adapter must accept. The
host writes one portable dialect, and an adapter lowers it to SQLite,
PostgreSQL or Cloudflare D1; a guest never sees engine-specific SQL.

## Versions

- [SQL contract v1](v1.md): the normative grammar for
  `sqlContractVersion = 1`, its limits, and the conformance vectors.

## Next steps

- [SQL database contract](../../adr/sql-database-contract.md): the
  decision behind host-resolved SQL and adapter-owned lowering.
- [Database](../../server/database.md): the adapters Bookclerk ships.
