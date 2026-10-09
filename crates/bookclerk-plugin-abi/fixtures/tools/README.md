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
| `valid-compat-date-future/` | `check` ok, and warns that load falls back to the workerd pin |
| `invalid-compat-date-shape/` | `check` fails (not a calendar `YYYY-MM-DD`) |
| `invalid-compat-flag/` | `check` fails (flag outside the Python allowlist) |
| `invalid-compat-experimental/` | `check` fails (`experimental` is host-only) |
| `invalid-python-flags-missing/` | `check` fails (Python guest missing the flag pair) |
| `invalid-flags-without-python/` | `check` fails (Python flags without a Python module) |
| `invalid-module-type/` | `check` fails (`[[modules]]` type disagrees with the extension) |
| `invalid-module-path/` | `check` fails (explicit `path` is absent even when `name` matches another file) |
| `valid-module-path-wins/` | `check` ok (`name = "helper.py"` with `path = "index.js"` is JS; path is the load-set key) |
| `valid-js-with-python-helper/` | `check` ok (JS main plus a declared `.py` helper; source lint follows the main extension) |
| `valid-module-name-only/` | `check` ok (`[[modules]]` omits `path`; `name` is the modules-directory source) |
| `invalid-module-ts/` | `check` fails (`.ts` main is not implemented yet) |
| `invalid-kv-oauth/` | `check` fails (`[[kv_namespaces]]` binding `OAUTH` collides with the host binding) |
| `invalid-kv-secret/` | `check` fails (KV binding collides with a custom `[secrets]` name) |
| `invalid-kv-work-fs/` | `check` fails (KV binding collides with a custom `[work_fs]` name) |
| `invalid-kv-oauth-name/` | `check` fails (KV binding collides with a custom `[oauth]` name) |
| `invalid-producer-database/` | `check` fails (event producer binding collides with `[[databases]]`) |
| `invalid-undeclared-python/` | `check` fails (disk-only `.py` is not a declared Python module, even if flags are added later) |
| `not-implemented-kv/` | `check` ok (`[[kv_namespaces]]` stays legal). Load and spawn fail with "not implemented yet". |
| `not-implemented-queues/` | `check` ok (`[queues]` stays legal). Load and spawn fail with "not implemented yet". |

Language-specific helpers (`sync-embed`, Python workerd flags) are covered in
each SDK's own tests against the Echo examples.

Canonical `fmt` output is produced by the Rust CLI (`bookclerk-plugin fmt`,
crate `bookclerk-plugin-tools`) and
compared by other SDKs via `fmt --check` against that gold file when present
(`*.fmt.toml`).
