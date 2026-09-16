# Migrating from classic Libation

Bookclerk can import and export classic **Libation Files** (Settings, accounts,
SQLite). Prefer the first-class import/export verbs; `migrate import` remains as
a hidden alias.

## Import Libation Files

```bash
export BOOKCLERK_FILES_DIR=./BookclerkFiles
bookclerk import libation --from ~/LibationFiles --force
# alias: bookclerk migrate import --from ~/LibationFiles --force
```

| Flag | Meaning |
| --- | --- |
| `--from` | Classic Libation Files directory (`BOOKCLERK_CLASSIC_FILES`) |
| `--force` | Overwrite existing `config.toml` |
| `--skip-auth` | No-op retained for CLI compatibility (credentials are never imported here) |
| `--dry-run` | Report only |

Imports typically include:

- Settings → `config.toml` (Widevine, lossy decrypt, folder/file templates,
  cover/cue/fix-up flags, …)
- `AccountsSettings.json` → account metadata in `library.db` (no credentials)
- `LibationContext.db` → Bookclerk `library.db`
- Naming templates and user metadata where present

IdentityTokens and other store credentials are **not** converted by migrate
(file-based auth leftovers are discarded). After import, re-authenticate with
`bookclerk auth login --source audible`, or import an audible-rs auth file via
`bookclerk auth import` (Audible plugin — see [sources.md](sources.md)).

Example AccountsSettings shape:
[`examples/AccountsSettings.example.json`](../examples/AccountsSettings.example.json).

## Export back to Libation Files

```bash
bookclerk export libation --path ./LibationFilesOut --force
```

Writes Settings.json, AccountsSettings.json (account metadata only), and
LibationContext.db for interoperability with classic tooling. Credential
material is never exported — it stays in `encrypted_secrets`.

## Native Bookclerk backup

Portable `.tar.gz` of the files directory (preferred for Bookclerk↔Bookclerk):

```bash
bookclerk export native --path ./bookclerk-backup.tar.gz
bookclerk import native --from ./bookclerk-backup.tar.gz --force
```

Optional export flags: `--include-plugin-manifests`, `--include-cache`,
`--include-logs`, `--include-plugin-databases`. Native backup is a **files-dir
archive** (config, `library.db`, optional extras). It is not a schema walk.

## Host schema backups (`bookclerk db`)

Schema state, in-place backups, and last-reversible downgrade live on
`bookclerk db`, not `export native` or `config database migrate` (backend
switch) or `plugins db` (binding list/drop). See
[ADR: schema versioning](adr/schema-versioning.md).

```bash
bookclerk db version
bookclerk db backup create --path ./recovery-point.tar.gz
bookclerk db backup create --include-plugin-databases
bookclerk db backup list
bookclerk db backup verify --from <id-or-archive>
bookclerk db backup prune
bookclerk db restore --from ./recovery-point.tar.gz
bookclerk db restore --from <recovery-point-id-or-timestamp>
bookclerk db migrate --to 1
bookclerk db downgrade
```

`db version` prints `uninitialized`, `unreleased@base<n>+<checksum>`, or
`frozen@<version>+<checksum>` — never “schema 0” for both empty and
applied development databases. `SCHEMA_VERSION = 0` means there are no
frozen revisions. Automatic `pre-migrate` recovery points land under
`$BOOKCLERK_FILES_DIR/backups/` (last five kept; reachability GC). Operator
`manual` backups are never pruned. Restore **replaces** schema and data using
canonical Bookclerk content (cross-adapter) and does not auto-migrate. Integrity
is verified before any destructive step. There is no production frozen v1 pack
yet: `downgrade` is a no-op until a reversible frozen step exists; use restore
for time travel.

`--include-plugin-databases` captures plugin-owned bindings from the
`plugin_databases` registry in portable Bookclerk format. Plugin schema
changes are host-mediated BookclerkSQL in the binding namespace (not a
plugin-owned migration framework). Each database unit is replaced completely; a
multi-database bundle is not one transaction across independent DBs.
Unsupported adapters fail closed.

Land new host DDL as already-separated `MigrationOp`s in `unreleased_ops`
(one BookclerkSQL statement per op; `Schema` vs `Data` is proven). A future
**release cut** may copy that bucket into
immutable `HostMigrationStep` ops (`Schema` / `Data`); until then the frozen
plan stays empty. Do not add a public plan version per PR. See
[ADR: schema versioning](adr/schema-versioning.md).

## Postgres (`copydb`)

```bash
bookclerk export postgres …     # preferred
bookclerk copydb …              # hidden alias
```

Default schema matches classic Libation EF Postgres; `--format flat` selects the
native flat layout. See `bookclerk export postgres --help`.

## After migrate

1. Review `$BOOKCLERK_FILES_DIR/config.toml` — enable only the sources you use.
2. Confirm auth: `bookclerk auth list` / `auth status`.
3. Set destination roots (`output.local` / `output.s3`).
4. Optionally `library scan --match-storage` if books already exist on disk/S3.
5. Keep `library.auto_acquire = false` until a dry run looks right.

Headless setting/CLI parity details: [libation-parity.md](libation-parity.md).
