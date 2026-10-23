# Generated API reference

This directory holds **generated** API HTML produced by
[`scripts/generate-api-docs.sh`](../../scripts/generate-api-docs.sh).

| Path | Generator | Source |
| --- | --- | --- |
| `rust/` | `cargo doc` | Workspace library crates |
| `typescript/` | TypeDoc | [`packages/plugin-sdk`](../../packages/plugin-sdk/) |
| `ui/` | TypeDoc | [`ui/src`](../../ui/src/) |
| `python/` | pdoc (Google docstrings) | [`packages/plugin-sdk-python`](../../packages/plugin-sdk-python/) |

Generated trees are **gitignored**. Clone the repo and run:

```bash
./scripts/generate-api-docs.sh
```

Then open `docs/api/rust/bookclerk_plugin_sdk/index.html` (or the TypeDoc /
pdoc `index.html` files) in a browser.

Inline comment conventions: [Code documentation (Google style)](../code-documentation.md).

## Publishing

| Channel | What consumers see |
| --- | --- |
| crates.io / docs.rs | Built from crate sources + `[package.metadata.docs.rs]` |
| npm | Package README + TypeDoc locally / linked from README |
| PyPI | Package README + pdoc locally / linked from README |

Regenerating here is the pre-publish smoke test that every public surface still
documents cleanly before a release cut.
