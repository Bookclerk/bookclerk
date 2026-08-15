# bookclerk-plugin-sdk (Python)

Python guest SDK for Bookclerk workerd plugins (`api_version = 2`).

| Import | Runtime |
| --- | --- |
| `from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js` | Workerd / Python Workers |

`bookclerk-workerd` injects `bookclerk_plugin_sdk.workerd` into the isolate —
authors do not vendor a relative filepath. Native guests use the Rust SDK
(`PluginRoot` / `serve`).

```bash
pip install -e packages/plugin-sdk-python
python -m bookclerk_plugin_sdk check .
python -m bookclerk_plugin_sdk package --out dist .
python -m bookclerk_plugin_sdk smoke .   # workerd: download pin, describe + health
```

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`).

## Workerd

```python
from bookclerk_plugin_sdk.workerd import BookclerkPlugin, Integration, js

class Default(BookclerkPlugin):
    async def describe(self):
        return js({
            "apiVersion": 2,
            "id": "my_plugin",
            "kind": "integration",
            "rpcFeatures": [],
            "scalarLimits": {
                "maxScalarBytes": 262144,
                "maxStreamWindowBytes": 1048576,
                "maxListPage": 256,
            },
        })
```

Declare Python Workers flags in `plugin.toml`:

```toml
api_version = 2
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
