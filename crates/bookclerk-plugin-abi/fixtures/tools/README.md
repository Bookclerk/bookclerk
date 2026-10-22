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
| `valid-compat-date-future/` | `check` ok, and warns that load falls back to the workerd pin (runners rewrite `compatibility_date` to the year after the pin) |
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
| `valid-producer-shares-oauth-name/` | `check` ok (an event producer may reuse a custom `[oauth]` binding name) |
| `invalid-undeclared-python/` | `check` fails (disk-only `.py` is not a declared Python module, even if flags are added later) |
| `valid-kv/` | `check` ok: a default `KV` row plus a named namespace with its own `max_value_bytes` / `max_keys`; materializes and spawns |
| `invalid-kv-namespace/` | `check` fails (`namespace` outside `[a-z0-9][a-z0-9._-]*`) |
| `invalid-producer-host-event/` | `check` fails (`[[events.producers]] type = "book_acquired"` is host-emitted) |
| `valid-queues/` | `check` ok (`[[queues.producers]]` send binding plus a `[[queues.consumers]]` trigger with a dead-letter queue) |

Language-specific helpers (`sync-embed`, Python workerd flags) are covered in
each SDK's own tests against the Echo examples.

Canonical `fmt` output is produced by the Rust CLI (`bookclerk-plugin fmt`,
crate `bookclerk-plugin-tools`) and
compared by other SDKs via `fmt --check` against that gold file when present
(`*.fmt.toml`).
