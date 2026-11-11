# Echo Integration (native Rust)

Reference **native** Bookclerk guest implementing [`PluginWorker`](../../crates/bookclerk-plugin-sdk)
(`api_version = 3`).
`cliDescribe` / `cliInvoke` (`ping --message`, `notes-add --body <text>
[--count N]` and `notes-list [--limit N]`) stay on the guest.

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md)
and [docs/plugins/README.md](../../docs/plugins/README.md).

```bash
cargo build -p bookclerk-plugin-echo-native-rust
# or: cargo stage-plugins --examples
bookclerk plugins approve echo_native_rust --yes
bookclerk plugins deploy echo_native_rust
bookclerk plugins echo_native_rust ping --message hi
```

## The notes database

`src/notes.rs` is a worked example of the
[database helpers](../../docs/plugins/database-limits.md#a-worked-example):
`database_migrations` registers a `notes` table on the `DB` binding, writes
are sized from `limits()` and split with `chunk()`, a note past
`maxCellBytes` is refused before it is sent, and `NotesReader` reads with
`iterate()` when the session streams and in key-ordered pages when it does
not. A refusal is handled by the cap `PluginError::limit()` names, and a
later batch that fails says how many notes the earlier ones stored.

`tests/` open the binding with `bookclerk_db_testkit::BindingDatabase` and
size every assertion from its limits. Run them under every database
profile:

```bash
cargo plugin -- test examples/plugins-echo-native-rust
```

Sibling examples: `plugins-echo-native-node`, `plugins-echo-native-python`,
`plugins-echo-workerd-ts`, `plugins-echo-workerd-python`, `plugins-echo-workerd-rust`.
