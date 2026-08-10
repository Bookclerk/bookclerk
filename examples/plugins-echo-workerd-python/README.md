# Echo Integration (workerd Python)

Full **Cloudflare Python Workers** Echo guest. Authors import the package:

```python
from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js
```

`bookclerk-workerd` injects that module (plus `python_workers` /
`disable_python_external_sdk`). Native guests use
`from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest` instead.

## Contract

- `handshake` / `health` / `diagnose` / `onEvent` / `cliDescribe` / `cliInvoke`
- Health detail: `echo workerd python plugin ready`

## Try it

```bash
cargo ensure-workerd
cargo build -p bookclerk-workerd
./scripts/test-workerd-echo.sh debug

python -m bookclerk_plugin_sdk check examples/plugins-echo-workerd-python
python -m bookclerk_plugin_sdk smoke examples/plugins-echo-workerd-python
python -m bookclerk_plugin_sdk package --out /tmp/out examples/plugins-echo-workerd-python
```

`smoke` downloads the pinned Cloudflare `workerd` (no Rust
`bookclerk-workerd` binary required). See
[docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).
