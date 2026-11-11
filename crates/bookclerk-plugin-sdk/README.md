# bookclerk-plugin-sdk

Guest-only Bookclerk plugin SDK for Rust authors of native and workerd
plugins (`api_version = 3`).

Depend on this crate from a standalone plugin workspace, not on
`bookclerk-plugin-host`. Implement `PluginWorker` and call `serve` (the
native Cap'n Proto runner), or use the workerd / npm bridge in
`src/workerd.rs`:

```toml
[dependencies]
bookclerk-plugin-sdk = { git = "https://github.com/Bookclerk/bookclerk", package = "bookclerk-plugin-sdk" }
# Database guests also need:
# bookclerk-plugin-sdk = { …, features = ["db"] }
```

The guide, with the author model, the features, the socket proxy and the
author CLI, is the
[Rust plugin SDK](https://bookclerk.app/docs/plugins/sdk/rust) page
([`docs/plugins/sdk/rust/README.md`](../../docs/plugins/sdk/rust/README.md)
in this repository). The crate's reference follows it on the same page.

## Database

With the `db` feature a guest takes its `[[databases]]` binding with
`DatabaseBinding::take_named_from_bindings` and uses it as on D1
(`prepare`, `bind`, `all`, `batch`), plus `limits()` and `chunk()` for
staying within the adapter's caps; a cap that is hit is a `PluginError`
with `PluginErrorCode::LimitExceeded`. Tests open the same binding with
`bookclerk_db_testkit::BindingDatabase`, and `bookclerk-plugin test .`
runs them under every capability profile. See
[Working within database limits](https://bookclerk.app/docs/plugins/database-limits)
([`docs/plugins/database-limits.md`](../../docs/plugins/database-limits.md)).

## API documentation

Rustdoc for this crate is generated with the workspace API docs:

```bash
./scripts/generate-api-docs.sh
# or
cargo doc -p bookclerk-plugin-sdk --no-deps --all-features --open
```

HTML lands under `target/api-docs/rust/`. Style expectations are in
[`docs/contributing/code-documentation.md`](../../docs/contributing/code-documentation.md).
