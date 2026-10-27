# Backups, updates, import and export

This page covers keeping recovery points, updating Bookclerk safely, and
moving a library in or out: to and from Libation, between Bookclerk
installs, and out to Postgres. For a step-by-step move from Libation, see
[Coming from Libation](../guide/from-libation.md).

## Recovery points

A **recovery point** is a backup of the library database's schema and
data, made with `bookclerk db`. Use recovery points before an update and
whenever you want a way back.

| Task | Command |
| --- | --- |
| Show the schema state | `bookclerk db version` |
| Make a recovery point | `bookclerk db backup create --path ./recovery-point.tar.gz` |
| Include plugin databases | `bookclerk db backup create --include-plugin-databases` |
| List recovery points | `bookclerk db backup list` |
| Check one | `bookclerk db backup verify --from <id-or-archive>` |
| Remove old automatic ones | `bookclerk db backup prune` |
| Restore from a file | `bookclerk db restore --from ./recovery-point.tar.gz` |
| Restore by id or time | `bookclerk db restore --from <recovery-point-id-or-timestamp>` |
| Migrate to a schema version | `bookclerk db migrate --to 1` |

Schema backups live only on `bookclerk db`. `export native` makes a files
archive, `config database migrate` switches backends, and `plugins db`
lists or drops plugin bindings.

### What the schema state means

`bookclerk db version` prints one of:

- `uninitialized`: no schema yet.
- `unreleased@base<n>+<checksum>`: a development schema.
- `frozen@<version>+<checksum>`: a released schema.

`SCHEMA_VERSION = 0` means there are no frozen revisions yet. There is no
released v1 schema yet either.

### What gets kept

- Bookclerk makes a **pre-migrate** recovery point automatically, under
  `$BOOKCLERK_FILES_DIR/backups/`. It keeps the last five, and removes
  ones nothing refers to.
- Recovery points you make yourself (`manual`) are never pruned.

### Restoring

> [!WARNING]
> A restore **replaces** the schema and the data. It does not migrate
> afterwards.

- Bookclerk checks the recovery point's integrity before it changes
  anything.
- The content is in Bookclerk's own format, so it restores onto a
  different database adapter too.
- `migrate --to <older>` can only step down through reversible frozen
  steps, and none exist yet. To go back in time, restore instead.

### Plugin databases

`--include-plugin-databases` also saves the databases plugins own (the
`plugin_databases` registry), in Bookclerk's portable format.

- Each database is replaced as a whole on restore. A bundle of several
  databases is not one transaction across them.
- An adapter that can't take part fails closed.
- Plugin schema changes run as host-mediated BookclerkSQL in the
  plugin's own namespace, not through a migration framework of the
  plugin's.

Contributors adding host schema changes: see
[ADR: schema versioning](../adr/schema-versioning.md).

## Updating Bookclerk

Bookclerk has no release yet, so its database layout is a development
layout. There is no upgrade path between two builds whose layouts differ.

A build opens a library whose layout matches it. Otherwise it refuses,
with `unreleased schema checksum … does not match this binary …; recreate
the database (cargo reset --yes)`, rather than reshape the library. See
[ADR: schema versioning](../adr/schema-versioning.md).

### Before you update

1. Make a recovery point with the build you are running now:

   ```sh
   bookclerk db backup create --path ./before-update.tar.gz
   ```

2. Keep a copy of `master.key`, and know your `BOOKCLERK_AUTH_PASSWORD`
   if you set one.
3. Optionally, save a book list that any build can read:

   ```sh
   bookclerk export libation --path ./LibationFilesOut --force
   ```

> [!CAUTION]
> Without `master.key` (and its password), sealed store sign-ins can't be
> read back.

### Update

1. Update the checkout with `git pull`.
2. Start Bookclerk again: `cargo dev --optional`, or rebuild the release
   binaries and restart the service.

### If the new build refuses the library

Pick one:

- **Go back** to the previous build. Your recovery point still restores
  there.
- **Start fresh.** Run `cargo reset --yes`, then bring the book list back
  with `bookclerk import libation --from ./LibationFilesOut`. Store
  sign-ins are never exported, so connect the stores again afterwards.

## Copy a whole install

A **native backup** is a `.tar.gz` of the files directory: config,
`library.db`, and any extras you add. Use it to move between Bookclerk
installs. It is an archive of files, not a walk of the schema.

```sh
bookclerk export native --path ./bookclerk-backup.tar.gz
bookclerk import native --from ./bookclerk-backup.tar.gz --force
```

Optional export flags add more: `--include-plugin-manifests`,
`--include-cache`, `--include-logs` and `--include-plugin-databases`.

## Import from Libation

Bookclerk imports classic **Libation Files**: settings, accounts and the
SQLite database.

```sh
export BOOKCLERK_FILES_DIR=./BookclerkFiles
bookclerk import libation --from ~/LibationFiles --force
```

`bookclerk migrate import` still works as a hidden alias.

| Flag | Meaning |
| --- | --- |
| `--from` | The Libation Files folder (or `BOOKCLERK_CLASSIC_FILES`) |
| `--force` | Overwrite an existing `config.toml` |
| `--skip-auth` | Does nothing; kept so old scripts still run |
| `--dry-run` | Report what would happen, change nothing |

### What comes across

- **Settings** go to `config.toml`: Widevine, lossy decrypt, folder and
  file templates, and cover, cue and fix-up flags. The packaging settings
  seed the `core.output` document when the database has none yet. If it
  has one, Bookclerk keeps it and warns.
- **`AccountsSettings.json`** becomes account details in `library.db`,
  without credentials.
- **`LibationContext.db`** becomes Bookclerk's `library.db`.
- **Naming templates and user metadata**, where present.

Example accounts file:
[`examples/AccountsSettings.example.json`](../../examples/AccountsSettings.example.json).

> [!IMPORTANT]
> Store credentials don't come across. IdentityTokens and other
> file-based sign-ins are discarded. After the import, connect Audible
> again from **Accounts**; see [Stores](stores.md).

### After the import

1. Run `bookclerk plugins list` and keep only the stores you use.
   `bookclerk plugins undeploy <id>` retires the rest on this host.
2. Check the accounts with `bookclerk library accounts` and
   `bookclerk library status`.
3. Check the destination with `bookclerk plugins config get local`. The
   import seeds the books folder there.
4. If books are already on disk or in S3, run
   `bookclerk library scan --match-storage`.
5. Keep `library.auto_acquire = false` until a dry run looks right.

How Bookclerk's settings and commands map to Libation's:
[Libation parity](../contributing/research/libation-parity.md).

## Export to Libation

```sh
bookclerk export libation --path ./LibationFilesOut --force
```

This writes `Settings.json`, `AccountsSettings.json` (account details
only) and `LibationContext.db`, for classic tools. Credentials are never
exported; they stay in `encrypted_secrets`.

## Export to Postgres

```sh
bookclerk export postgres …
```

`bookclerk copydb` is a hidden alias. The default layout matches classic
Libation's EF Postgres schema; `--format flat` picks Bookclerk's flat
layout. See `bookclerk export postgres --help`.

## Next steps

- [Database](database.md): backends, and moving between them.
- [Security checklist](security.md): what to set up before going public.
- [CLI reference](cli-reference.md): every command and flag.
