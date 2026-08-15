# Echo Integration (id `echo_native_python`)

This example now validates **workerd** hosting (`runtime = "workerd"`,
`api_version = 2`), not a Python Cap'n Proto stack. The guest class lives in
[`modules/plugin.py`](modules/plugin.py) and matches
[`plugins-echo-workerd-python`](../plugins-echo-workerd-python/).

```bash
cargo ensure-workerd
cargo stage-plugins --examples --skip-build
python -m bookclerk_plugin_sdk check examples/plugins-echo-native-python
```

Historical PyInstaller sketches (`echo_plugin.py`, `requirements-dev.txt`,
`.github/workflows/package.yml`) are **not** the guest entry. Keep them only as
publisher notes; they do not speak the product ABI.

Sibling examples: `plugins-echo-native-rust`, `plugins-echo-native-node`,
`plugins-echo-workerd-*`.
