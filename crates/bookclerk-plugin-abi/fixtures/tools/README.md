# Plugin author-tools conformance fixtures

Shared corpus for Rust / TypeScript / Python SDK `check`, `fmt`, and `package`
implementations. Each SDK must accept the `valid-*` trees and reject
`invalid-*` with a non-zero exit (or equivalent error).

| Path | Expectation |
| --- | --- |
| `valid-native/` | `check` ok; `package` includes `plugin.toml` + binary named by `command` |
| `valid-workerd/` | `check` ok; `package` includes `plugin.toml` + `modules/` (imports `@bookclerk/plugin-sdk/workerd`) |
| `valid-logo-url/` | `check` ok (`logo` https URL) |
| `valid-logo-path/` | `check` ok (`logo` relative image under plugin root) |
| `invalid-outbound-no-domains/` | `check` fails (outbound without domains) |
| `invalid-logo-javascript/` | `check` fails (`javascript:` logo) |
| `invalid-logo-parent/` | `check` fails (`..` in embedded logo path) |

Language-specific helpers (`sync-embed`, Python workerd flags) are covered in
each SDK's own tests against the Echo examples.

Canonical `fmt` output is produced by the Rust SDK (`bookclerk-plugin fmt`) and
compared by other SDKs via `fmt --check` against that gold file when present
(`*.fmt.toml`).
