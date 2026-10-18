# Repository instructions

Follow [AGENTS.md](../AGENTS.md), in particular **Contributing workflow** and
**Review guidelines**.

When reviewing a pull request:

- Read its **Intent**, **In scope** and **Out of scope** first and judge the
  change against them. Mark findings that are substantially different from the
  pull request's intent as out of scope (they become follow-up issues).
- Treat these invariants as blocking:
  - No in-process fallback for plugins, database adapters, or codecs; missing
    helpers fail closed (`media.isolation` / `plugins.isolation = "required"`).
  - Secrets never on argv, in env vars forwarded to guests, or in logs; new
    secret paths register with redaction.
  - Guests get only their declared capabilities; network access only to approved
    `[capabilities.network]` domains.
  - Host SQL goes through the canonical SQL-v1 contract
    ([`docs/sql-contract/v1.md`](../docs/sql-contract/v1.md)), never engine-specific
    SQL.
  - `plugin.capnp` is append-only (new ordinals only) and changes ship with the
    TS SDK.
  - Plugin configuration lives in the database; no new `[sources.*]`-style TOML
    tables or `BOOKCLERK_*` env overrides.
  - Prefer a plugin entrypoint or job/event over host special-casing.
- Formatting and lints are enforced by rustfmt and Clippy; don't comment on
  them.
