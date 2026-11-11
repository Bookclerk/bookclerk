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
- `Default.database_migrations(binding)`: creates the `notes` table on the
  `DB` binding (`[[databases]]` in `plugin.toml`) before the host grants it.
- `Cli(CliEntrypoint)`: the `cli` entrypoint — `describe()` returns the CLI
  schema, `invoke(params)` answers `ping --message <text>`,
  `notes-add --body <text> [--count N]` and `notes-list [--limit N]`.
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

## The notes database

`modules/notes.py` is a worked example of the
[database helpers](../../docs/plugins/database-limits.md#a-worked-example):
it sizes writes from `limits()`, splits them with `chunk()`, refuses a note
past `maxCellBytes` before sending it, reads with `iterate()` when the
session streams and in key-ordered pages when it does not, and handles a
refusal by `PluginError.limit`. It imports only the SDK, so it runs the
same in the isolate and under CPython.

`tests/` drive the module and the CLI against the binding
`bookclerk-db-testkit` serves. Run them under every capability profile:

```bash
cargo build -p bookclerk-db-testkit
PYTHONPATH=packages/plugin-sdk-python/src cargo plugin -- test examples/plugins-echo-workerd-python
```

`bookclerk-plugin test` picks pytest from `pyproject.toml` and runs
`python3 -m pytest`, so that interpreter needs pytest and the SDK (here
from its source tree through `PYTHONPATH`).

`smoke` downloads the pinned Cloudflare `workerd` (no Rust
`bookclerk-workerd` binary required). See
[docs/adr/plugin-workers-rpc-workerd.md](../../docs/adr/plugin-workers-rpc-workerd.md).
