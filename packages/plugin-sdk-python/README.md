# bookclerk-plugin-sdk (Python)

Python guest SDK for Bookclerk workerd plugins (`api_version = 3`).

A plugin is a Python Worker: a module-level `Default` class extending
`BookclerkEntrypoint`, plus one class per named entrypoint in
`plugin.toml`. `bookclerk-workerd` injects `bookclerk_plugin_sdk.workerd`
into the isolate, so authors import it rather than vendor a path.

```bash
pip install bookclerk-plugin-sdk          # from PyPI
pip install -e packages/plugin-sdk-python # from this repository
python -m bookclerk_plugin_sdk check .
python -m bookclerk_plugin_sdk types .            # writes bookclerk_configuration.py
python -m bookclerk_plugin_sdk package --out dist .
python -m bookclerk_plugin_sdk smoke .   # workerd: download pin, describe + health
```

The guide, with the author model, the triggers, the `env` bindings and the
author CLI, is the
[Python plugin SDK](https://bookclerk.app/docs/plugins/sdk/python) page
([`docs/plugins/sdk/python/README.md`](../../docs/plugins/sdk/python/README.md)
in this repository). The package's reference follows it on the same page.
A complete plugin is
[`examples/plugins-echo-workerd-python`](../../examples/plugins-echo-workerd-python/).

## Developing the SDK

`uv.lock` pins the `dev` and `docs` extras (the SDK itself has no runtime
dependencies); CI installs from it and the dependency scans read it. CI does
not build the SDK (its build backend is not in the lockfile) and imports it
from `src/` instead:

```bash
uv sync --project packages/plugin-sdk-python --locked --all-extras --no-install-project
PYTHONPATH=packages/plugin-sdk-python/src \
  uv run --project packages/plugin-sdk-python --no-sync pytest packages/plugin-sdk-python/tests
```

After changing dependencies in `pyproject.toml`, run `uv lock` in
`packages/plugin-sdk-python` and commit the updated `uv.lock`.

## API docs

Public APIs use [Google-style docstrings](https://google.github.io/styleguide/pyguide.html#38-comments-and-docstrings).
Generate HTML with pdoc (installed from `uv.lock`):

```bash
./scripts/generate-api-docs.sh --python
```
