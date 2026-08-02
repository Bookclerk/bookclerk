# Echo plugin — Python / PyInstaller (experimental)

**SUPPORT tier: Experimental (Tier 2).** Not supported at Rust / `bookclerk-plugin-sdk`
parity. Nuitka (or other freezers) may be explored later; v1 sketch is PyInstaller
onefile.

Minimal JSON-RPC stdio guest matching the Rust Echo integration
(`api_version = 1`, `id = echo`, `kind = integration`, `network = none`) with a
`cli.invoke` `ping` command.

## Packaging notes

| Constraint | Implication |
| --- | --- |
| **No cross-compile** | Build on each OS/arch runner you publish (`ubuntu-latest`, `macos-latest`, `windows-latest`) |
| **glibc coupling** | Linux wheels/binaries from `ubuntu-latest` need a matching (or newer) glibc; **Alpine/musl is out of scope** |
| Artifact size | Onefile bundles are larger than Rust guests — expected |

Bookclerk never runs `pip` / Python on the operator machine — only the frozen
binary ships in the archive.

## Local run (dev)

```bash
cd examples/plugins-echo-py
python3 echo_plugin.py
```

Build a onefile binary (dev machine only):

```bash
pip install -r requirements-dev.txt
pyinstaller --onefile --name bookclerk-plugin-echo-py echo_plugin.py
# → dist/bookclerk-plugin-echo-py[.exe]
```

Pack `plugin.toml` + binary as
`bookclerk-plugin-echo-py-{version}-{bookclerk_target}.{tar.gz|zip}` and publish
`SHA256SUMS`. See [`.github/workflows/package.yml`](.github/workflows/package.yml).

## Related

- Rust Echo: `crates/bookclerk-plugin-examples/echo-integration`
- Publisher reusable workflow: [`../plugin-publisher/`](../plugin-publisher/)
- Protocol: [`../../docs/plugins.md`](../../docs/plugins.md)
