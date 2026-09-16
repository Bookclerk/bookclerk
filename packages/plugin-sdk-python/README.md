# bookclerk-plugin-sdk (Python)

Python guest SDK for Bookclerk workerd plugins (`api_version = 3`).

| Import | Runtime |
| --- | --- |
| `from bookclerk_plugin_sdk.workerd import BookclerkEntrypoint, CliEntrypoint, js` | Workerd / Python Workers |

`bookclerk-workerd` injects `bookclerk_plugin_sdk.workerd` into the isolate —
authors do not vendor a relative filepath. Native guests use the Rust SDK
(`PluginWorker` / `serve`).

```bash
pip install -e packages/plugin-sdk-python
python -m bookclerk_plugin_sdk check .
python -m bookclerk_plugin_sdk types .            # writes bookclerk_configuration.py
python -m bookclerk_plugin_sdk package --out dist .
python -m bookclerk_plugin_sdk smoke .   # workerd: download pin, describe + health
```

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`).

## Workerd

A plugin is a Python Worker. The module-level ``Default`` class extends
``BookclerkEntrypoint`` and implements the triggers ``plugin.toml`` declares
(``event(batch)`` for ``[[events.consumers]]``, ``job(job)`` for
``[triggers] jobs``, ``database_migrations(binding)`` for ``[[databases]]``).
Every named entrypoint in ``entrypoints = [...]`` is a module-level class with
the matching name extending its base: ``Storefront(StorefrontEntrypoint)``,
``Storage(StorageEntrypoint)``, ``RemoteLibrary(RemoteLibraryEntrypoint)``,
``DatabaseAdapter(DatabaseAdapterEntrypoint)``, ``Cli(CliEntrypoint)``,
``Oidc(OidcEntrypoint)``.

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

Identity (``apiVersion``, ``id``, capabilities) comes from ``plugin.toml``; an
optional ``describe()`` on ``Default`` only refines ``displayName`` and similar
fields. Each ``EventMessage`` records one outcome — ``ack()``,
``retry(retry_at=..., delay_seconds=..., reason=...)``, ``reject(reason)``,
``dead_letter(reason)``, or ``suspend(checkpoint=..., wake_at=...)`` — and the
first call wins; untouched messages ack on return and retry on raise. A
``job(job)`` handler returns to complete, raises to reject, or calls
``job.suspend(...)`` / ``job.retry_later(...)``.

Granted bindings arrive on ``self.env`` per invocation: ``CONFIG``,
``SECRETS``, ``EVENTS``, ``WORK_FS``, and one entry per ``[[databases]]``
binding. ``python -m bookclerk_plugin_sdk types .`` writes a typed ``Env``
protocol (``bookclerk_configuration.py``) from ``plugin.toml``.

Declare Python Workers flags in `plugin.toml`:

```toml
api_version = 3
runtime = "workerd"
[workerd]
compatibility_flags = ["python_workers", "disable_python_external_sdk"]
main_module = "plugin.py"
```

See [`examples/plugins-echo-workerd-python`](../../examples/plugins-echo-workerd-python/).

## API docs

Public APIs use [Google-style docstrings](https://google.github.io/styleguide/pyguide.html#38-comments-and-docstrings).
Generate HTML with pdoc:

```bash
pip install -e "packages/plugin-sdk-python[docs]"
```
