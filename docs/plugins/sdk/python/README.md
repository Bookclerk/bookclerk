# Python plugin SDK

This page shows how to write a Bookclerk plugin in Python with
`bookclerk-plugin-sdk` (`api_version = 3`). A Python plugin is a Python
Worker in a workerd isolate; `bookclerk-workerd` injects
`bookclerk_plugin_sdk.workerd` and the modules it uses (including
`bookclerk_plugin_sdk.db_value` for database bindings) into the isolate, so
you import them rather than vendor a path.

| Import | What it holds |
| --- | --- |
| `bookclerk_plugin_sdk.workerd` | `BookclerkEntrypoint`, `CliEntrypoint` and the `js` bridge |
| `python -m bookclerk_plugin_sdk` | The author CLI |

## Install

```bash
pip install bookclerk-plugin-sdk          # from PyPI
pip install -e packages/plugin-sdk-python # from this repository
```

Declare Python Workers in `plugin.toml`:

```toml
api_version = 3
runtime = "workerd"
[workerd]
compatibility_flags = ["python_workers", "disable_python_external_sdk"]
main_module = "plugin.py"
```

## Author model

The module-level `Default` class extends `BookclerkEntrypoint` and
implements the triggers `plugin.toml` declares: `event(batch)` for
`[[events.consumers]]`, `job(job)` for `[triggers] jobs`,
`scheduled(controller)` for `[triggers] crons` and
`database_migrations(binding)` for `[[databases]]`. Every named
entrypoint in `entrypoints = [...]` is a module-level class with the
matching name extending its base: `Storefront(StorefrontEntrypoint)`,
`Storage(StorageEntrypoint)`, `RemoteLibrary(RemoteLibraryEntrypoint)`,
`DatabaseAdapter(DatabaseAdapterEntrypoint)`, `Cli(CliEntrypoint)`,
`Oidc(OidcEntrypoint)`, `Metadata(MetadataEntrypoint)` and
`Covers(CoverEntrypoint)`.

```python
from bookclerk_plugin_sdk.workerd import (
    BookclerkEntrypoint,
    CliEntrypoint,
    EventBatch,
    cli_args,
    json_payload,
)


class Cli(CliEntrypoint):
    async def describe(self):
        return {"commands": [{"name": "ping", "about": "Probe", "args": []}]}

    async def invoke(self, params):
        message = cli_args(params).get("message", "hi")
        return {
            "exitCode": 0,
            "stdout": f"pong: {message}\n",
            "stderr": "",
            "payload": json_payload({"pong": message}),
        }


class Default(BookclerkEntrypoint):
    async def event(self, batch: EventBatch):
        for msg in batch.messages:
            if msg.type != "book_acquired":
                msg.reject(f"unexpected {msg.type}")
                continue
            print(f"saw {msg.json().get('titleId', '')} via {self.env.CONFIG}")
            msg.ack()
```

Identity (`apiVersion`, `id`, capabilities) comes from `plugin.toml`; an
optional `describe()` on `Default` only refines `displayName` and similar
fields.

A database adapter runs each host batch through `execute_adapter_batch`
or `execute_adapter_batch_streamed` (in `bookclerk_plugin_sdk.db_value`),
which stores the guest's replay receipt before `COMMIT` and, for a
streamed reply, rolls back when the frames do not fit one scalar or the
adapter's advertised streamed budget (`execute_stream_limits(caps)`) (see
[Isolate adapter batches](../../sql-contract/v1.md#isolate-adapter-batches-and-replay-receipts)).

## Triggers

Each `EventMessage` records one outcome, `ack()`, `retry(retry_at=...,
delay_seconds=..., reason=...)`, `reject(reason)`, `dead_letter(reason)`
or `suspend(checkpoint=..., wake_at=...)`, and the first call wins.
Untouched messages ack on return and retry on raise. A `job(job)` handler
returns to complete, raises to reject, or calls `job.suspend(...)` or
`job.retry_later(...)`.

## The `env` bindings

Granted bindings arrive on `self.env` per invocation: `CONFIG`,
`SECRETS`, `EVENTS`, `WORK_FS`, and one entry per `[[databases]]`
binding. A binding's `await limits()` returns the `DbCapabilities` its
session negotiated; a request or result over one of them raises a
`PluginError` whose `code` is `limit_exceeded` and whose `limit` names
the cap. On a host that predates `limits()` the call raises
`unsupported`. `all()` reads through the session's streamed budget when
it advertises one, `prepare(sql).iterate()` yields rows one at a time
from the same read, `db.chunk(statements)` splits a long list into
batches that each fit one request, and `db.write_chunks(...)` commits those
batches resumably; see
[Working within database limits](../../database-limits.md). A
storefront's `scan` can hand its paging to `scan_page(params, fetch)`
(`from bookclerk_plugin_sdk.scan_page import scan_page`, which also works
inside a workerd isolate), which slices the store's own pages into replies
the host accepts; see [Paged scans](../../abi.md#paged-scans). Generate a typed `Env` protocol from `plugin.toml`:

```bash
python -m bookclerk_plugin_sdk types .   # writes bookclerk_configuration.py
```

## Testing database code

`create_test_database()` from `bookclerk_plugin_sdk.testkit` gives a
pytest suite the same binding a host grants, served by the
`bookclerk-db-testkit` executable under one capability profile with the
plugin's `database_migrations` applied. `bookclerk-plugin test .` runs the
suite once per profile (`first-party`, `d1` and `minimum`):

```python
from bookclerk_plugin_sdk.testkit import create_test_database

async with await create_test_database(migrations=MIGRATIONS) as db:  # profile from `bookclerk-plugin test`
    await db.binding.prepare("INSERT INTO notes (body) VALUES (?)").bind(body).run()
```

The `echo_workerd_python` example (`examples/plugins-echo-workerd-python/`)
is a worked plugin: a `notes` table it writes with `chunk()`, lists with
`iterate()` or key-paged reads, sizes with `limits()`, and tests under
every profile. A plugin's own `pyproject.toml` is what makes
`bookclerk-plugin test` run pytest. See
[Testing against the limits](../../database-limits.md#testing-against-the-limits).

## Author tools

```bash
python -m bookclerk_plugin_sdk check .
python -m bookclerk_plugin_sdk types .
python -m bookclerk_plugin_sdk package --out dist .
python -m bookclerk_plugin_sdk smoke .   # download the pinned workerd, then describe + health
```

`smoke` does not need a built `bookclerk-workerd`. It downloads the
pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override
with `BOOKCLERK_WORKERD_CACHE` or `BOOKCLERK_WORKERD_BIN`).

## Next steps

- [plugin.toml](../../manifest.md): declare the entrypoints and
  triggers the classes above implement.
- [Entrypoints, events and the ABI](../../abi.md): every method and
  event in detail.
- [`examples/plugins-echo-workerd-python`](../../../../examples/plugins-echo-workerd-python/):
  a complete Python plugin.

## Reference

The sections below are generated from the SDK's source: one page per
module, and what the package itself exports.
