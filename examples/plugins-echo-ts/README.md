# Echo plugin — TypeScript / Node SEA (experimental)

**SUPPORT tier: Experimental (Tier 2).** Not supported at Rust / `bookclerk-plugin-sdk`
parity. Publishers own Node CVE updates and SEA toolchain churn.

Minimal JSON-RPC stdio guest matching the Rust Echo integration
(`api_version = 1`, `id = echo`, `kind = integration`, `network = none`) with a
`cli.invoke` `ping` command.

## Platform gaps

Node’s [Single Executable Applications](https://nodejs.org/api/single-executable-applications.html)
CI covers roughly:

| Target | Status |
| --- | --- |
| Linux glibc (`linux-x64-gnu`) | Experimental — try SEA |
| Windows (`windows-x64`) | Experimental — try SEA |
| macOS arm64 (`macos-arm64`) | Experimental — try SEA |
| **Alpine / musl** | **Unsupported gap** |
| **macOS x64** | **Weak / not in Node SEA CI** — do not claim support |

Until this package passes Bookclerk conformance on the OSes you ship, keep it
labeled experimental and prefer the Rust Echo example for real installs.

## Local run (dev, not jailed SEA)

```bash
cd examples/plugins-echo-ts
node src/echo.mjs
# Host speaks newline-delimited JSON-RPC on stdin; responses on stdout.
```

Install layout when packaged: `plugin.toml` + `bookclerk-plugin-echo-ts` under
`$BOOKCLERK_FILES_DIR/plugins/echo/`.

## Packaging sketch

See [`.github/workflows/package.yml`](.github/workflows/package.yml) and
`scripts/build-sea.mjs`. Flow:

1. `node --experimental-sea-config sea-config.json` → `sea-prep.blob`
2. Copy the Node binary; on macOS remove the signature
3. Inject the blob with `postject` (flags per current Node docs)
4. Archive as `{crate}-{version}-{bookclerk_target}.{tar.gz|zip}` + `SHA256SUMS`

Bookclerk never runs `npm` / Node on the operator machine — only the SEA binary
ships in the archive.

## Related

- Rust Echo: `crates/bookclerk-plugin-examples/echo-integration`
- Publisher reusable workflow: [`../plugin-publisher/`](../plugin-publisher/)
- Protocol: [`../../docs/plugins.md`](../../docs/plugins.md)
