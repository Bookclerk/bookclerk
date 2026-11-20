# Author-surface fixture crates

Compile-time proof of the public plugin-author API boundary, driven by
`scripts/check-author-surface.sh`.

Both packages are **excluded from the workspace** on purpose: workspace
feature unification would otherwise leak the `bookclerk-plugin-abi` `host`
feature (enabled by first-party host crates) into this graph and make the
negative cases silently compile.

- `positive/` — depends on `bookclerk-plugin-sdk` with the documented
  author features (`db`). Importing and using the intended author API
  (`Database`, `AdapterDatabaseSession`, `DbCapabilities`,
  `ExecuteRequest` / `ExecuteReply`, `DatabaseBinding`,
  `database_adapter::{errors, migrate}`) must `cargo check` cleanly.
- `negative/` — depends on `bookclerk-plugin-sdk` and
  `bookclerk-plugin-abi` with **default features only**. Each file under
  `cases/` names one host-private symbol; the driver script swaps it into
  `src/case.rs` and requires `cargo check` to **fail** with an
  unreachable-symbol error (unresolved import, path, name or type, a
  missing method, or a private item) and no other error. A case that
  fails for any other reason, such as a syntax error or a warning promoted
  to an error, fails the check even next to an expected error, so it can't
  hide a leak.

Run locally:

```bash
./scripts/check-author-surface.sh
```
