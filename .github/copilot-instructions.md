# Repository instructions

Follow [AGENTS.md](../AGENTS.md), in particular **Contributing workflow** and
**Review guidelines**.

When reviewing a pull request:

- Read its **Intent**, **In scope** and **Out of scope** first and judge the
  change against them. Mark findings that are substantially different from the
  pull request's intent as out of scope (they become follow-up issues).
- Treat these invariants as blocking:
  - No in-process fallback for plugins, database adapters, or codecs.
  - Secrets never on argv, in env vars forwarded to guests, or in logs.
  - Guests get only their declared capabilities.
  - Host SQL uses the canonical SQL-v1 contract (`docs/sql-contract/v1.md`).
  - `plugin.capnp` is append-only and changes ship with the TS SDK.
  - Plugin configuration lives in the database, not TOML tables or env vars.
- Formatting and lints are enforced by rustfmt and Clippy; don't comment on
  them.
