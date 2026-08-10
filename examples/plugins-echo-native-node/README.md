# Echo Integration (native Node)

Reference **native** Bookclerk guest. Subclasses `BookclerkPlugin` and runs with
`BookclerkPluginGuest.serve` (`api_version = 1`, id `echo-native-node`).

**Dev / CI staging** (`cargo stage-plugins --examples`) installs a small shell
launcher that runs a **vendored** `runtime/node` (hardlinked/copied from
`BOOKCLERK_NODE` or `PATH`) against `src/echo.mjs` with `@bookclerk/plugin-sdk/native`
under `sdk/`. Vendoring keeps the interpreter inside the guest install tree so
Landlock can exec it — host toolcache paths are outside jail `system_paths`.
Requires `npm run build` in `packages/plugin-sdk` first.

**Publisher path:** experimental Node SEA via `npm run sea:build` (see
`scripts/build-sea.mjs`) — pack `plugin.toml` + the SEA binary as
`bookclerk-plugin-echo-native-node`.

```bash
cd packages/plugin-sdk && npm run build
cd examples/plugins-echo-native-node
node src/echo.mjs
# or after staging:
# target/plugin-artifacts/echo-native-node/bookclerk-plugin-echo-native-node
```

Sibling examples: `plugins-echo-native-rust`, `plugins-echo-native-python`,
`plugins-echo-workerd-*`.
