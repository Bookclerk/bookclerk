# Echo Integration (native Rust)

Reference **native** Bookclerk guest using [`BookclerkPlugin`](../../crates/bookclerk-plugin-sdk)
+ [`BookclerkPluginGuest`](../../crates/bookclerk-plugin-sdk) (`api_version = 1`).

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md)
and [docs/plugins.md](../../docs/plugins.md).

```bash
cargo build -p bookclerk-plugin-echo-native-rust
# or: cargo stage-plugins --examples
bookclerk plugins approve echo_native_rust --yes
bookclerk plugins enable echo_native_rust
bookclerk plugins echo_native_rust ping --message hi
```

Sibling examples: `plugins-echo-native-node`, `plugins-echo-native-python`,
`plugins-echo-workerd-ts`, `plugins-echo-workerd-python`, `plugins-echo-workerd-rust`.
