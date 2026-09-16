# Echo Integration (workerd Python)

Full **Cloudflare Python Workers** Echo guest. Authors import the package:

```python
from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, cli_args, json_payload
```

`bookclerk-workerd` injects that module (plus `python_workers` /
`disable_python_external_sdk`). Native guests use the Rust SDK
(`PluginWorker` / `serve`) instead.

## Contract

- `Default(BookclerkEntrypoint)`: optional `describe()` refinement plus the
  `event(batch)` trigger for the `book_acquired` consumer in `plugin.toml`
  (`msg.ack()` / `msg.reject(...)` per message).
- `Cli(CliEntrypoint)`: the `cli` entrypoint — `describe()` returns the CLI
  schema, `invoke(params)` answers `ping --message <text>`.
- `python -m bookclerk_plugin_sdk types .` generates a typed `Env` protocol
  from `plugin.toml`.

## Try it

```bash
cargo ensure-workerd
cargo build -p bookclerk-workerd
./scripts/test-workerd-echo.sh debug

python -m bookclerk_plugin_sdk check examples/plugins-echo-workerd-python
python -m bookclerk_plugin_sdk types examples/plugins-echo-workerd-python
python -m bookclerk_plugin_sdk smoke examples/plugins-echo-workerd-python
python -m bookclerk_plugin_sdk package --out /tmp/out examples/plugins-echo-workerd-python
```

`smoke` downloads the pinned Cloudflare `workerd` (no Rust
`bookclerk-workerd` binary required). See
[docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).
