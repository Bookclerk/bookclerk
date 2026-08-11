# Echo Integration (native Python)

Reference **native** Bookclerk guest. Subclasses `BookclerkPlugin` and runs with
`BookclerkPluginGuest.serve` (`api_version = 1`, id `echo_native_python`).

**Dev / CI staging** (`cargo stage-plugins --examples`) installs a small shell
launcher that runs `python3 echo_plugin.py` with a vendored
`bookclerk_plugin_sdk` under `sdk/`. Override the interpreter with
`BOOKCLERK_PYTHON`.

**Publisher path:** experimental PyInstaller onefile — pack `plugin.toml` +
`bookclerk-plugin-echo-native-python`.

```bash
cd examples/plugins-echo-native-python
python3 echo_plugin.py
pip install -r requirements-dev.txt
pyinstaller --onefile --name bookclerk-plugin-echo-native-python echo_plugin.py
```

Sibling examples: `plugins-echo-native-rust`, `plugins-echo-native-node`,
`plugins-echo-workerd-*`.
