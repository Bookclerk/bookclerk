# Echo Integration (workerd Rust / Wasm)

Full **workerd** Echo guest: Rust business logic compiled to Wasm
([`src/lib.rs`](src/lib.rs)), plus JS that imports the package:

```js
import { BookclerkPlugin, Integration } from "@bookclerk/plugin-sdk/workerd";
```

(`bookclerk-workerd` injects that module — not a relative embed path.)

Native counterpart: [`plugins-echo-native-rust`](../plugins-echo-native-rust/)
(`PluginRoot` / `serve`). Health detail:
`echo workerd rust wasm plugin ready`.

## Layout

```text
plugin.toml
modules/index.js              # package import + wasm glue
modules/pkg/*.js + *.wasm     # wasm-bindgen output (committed; rebuild below)
src/lib.rs                    # ABI-typed dispatch
```

## Rebuild Wasm glue

Requires `wasm32-unknown-unknown` and `wasm-bindgen-cli`:

```bash
./examples/plugins-echo-workerd-rust/build-wasm.sh
```

Then stage / smoke:

```bash
cargo ensure-workerd
cargo build -p bookclerk-workerd
./scripts/test-workerd-echo.sh debug

# Out-of-tree author smoke (SDK tools feature; library uses bookclerk-workerd):
cargo plugin -- smoke examples/plugins-echo-workerd-rust
```

See [docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).
