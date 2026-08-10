# bookclerk-plugin-sdk (Python)

Minimal Python guest SDK for Bookclerk plugins — **dual-stack** native + workerd.

| Import | Runtime |
| --- | --- |
| `from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest` | Native stdio Workers RPC |
| `from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js` | Workerd / Python Workers |

`bookclerk-workerd` injects `bookclerk_plugin_sdk.workerd` into the isolate —
authors do not vendor a relative filepath.

```bash
pip install -e packages/plugin-sdk-python
python -m bookclerk_plugin_sdk check .
python -m bookclerk_plugin_sdk package --out dist .
python -m bookclerk_plugin_sdk smoke .   # workerd: download pin, handshake + health
```

`smoke` does **not** need a built Rust `bookclerk-workerd` binary. It downloads
the pinned Cloudflare `workerd` into `~/.cache/bookclerk/workerd` (override with
`BOOKCLERK_WORKERD_CACHE` / `BOOKCLERK_WORKERD_BIN`), materializes Cap’n Proto +
bridge, then POSTs `handshake` / `health` to `/rpc`.

## Workerd

```python
from bookclerk_plugin_sdk.workerd import BookclerkPlugin, js

class Default(BookclerkPlugin):
    async def handshake(self, _params=None):
        return js({
            "apiVersion": 1,
            "id": "my-plugin",
            "kind": "integration",
            "capabilities": ["health"],
        })
```

Declare Python Workers flags in `plugin.toml`:

```toml
[workerd]
compatibility_flags = ["python_workers", "disable_python_external_sdk"]
main_module = "plugin.py"
```

## Native

```python
from bookclerk_plugin_sdk import BookclerkPlugin, BookclerkPluginGuest

class Echo(BookclerkPlugin):
    def handshake(self, _params):
        return {"apiVersion": 1, "id": "my-plugin", "kind": "integration", "capabilities": ["health"]}

if __name__ == "__main__":
    BookclerkPluginGuest.serve(Echo())
```

See [`examples/plugins-echo-workerd-python`](../../examples/plugins-echo-workerd-python/)
and [`examples/plugins-echo-native-python`](../../examples/plugins-echo-native-python/).
