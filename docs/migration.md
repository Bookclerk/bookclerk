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
| `--force` | Overwrite existing `config.toml` and auth files |
| `--skip-auth` | Import account metadata / library without writing `*.audible.auth` |
| `--dry-run` | Report only |

Imports typically include:

- Settings → `config.toml` (Widevine, lossy decrypt, folder/file templates,
  cover/cue/fix-up flags, …)
- `AccountsSettings.json` / auth material → `Accounts/`
- `LibationContext.db` → Bookclerk `library.db`
- Naming templates and user metadata where present

Example AccountsSettings shape:
[`examples/AccountsSettings.example.json`](../examples/AccountsSettings.example.json).

## Export back to Libation Files

```bash
bookclerk export libation --path ./LibationFilesOut --force
```

Writes Settings.json, AccountsSettings.json, and LibationContext.db for
interoperability with classic tooling.

## Native Bookclerk backup

Portable `.tar.gz` of the files directory (preferred for Bookclerk↔Bookclerk):

```bash
bookclerk export native --path ./bookclerk-backup.tar.gz
bookclerk import native --from ./bookclerk-backup.tar.gz --force
```

Optional export flags: `--include-plugin-manifests`, `--include-cache`,
`--include-logs`.

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
