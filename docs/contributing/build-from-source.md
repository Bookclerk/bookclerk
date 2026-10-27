# Build from source and use the CLI

This page shows how to build the Bookclerk binaries yourself and drive
Bookclerk from the command line. For a first start in the web app, see
[Getting started](../guide/getting-started.md).

## Prerequisites

- Rust stable (see `rust-toolchain.toml`; edition 2021, with the MSRV in
  the workspace).
- Network access to the stores you intend to use.
- Optionally, Docker, if you prefer the packaged daemon image.

No `ffmpeg` or `aaxclean-cli` binaries are needed. Decrypting, remuxing,
MP3 encoding, metadata fix-up and chapter splitting are native Rust.

## Build

```sh
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd
export PATH="$PWD/target/release:$PATH"
# binaries: target/release/bookclerk  target/release/bookclerkd
#           target/release/bookclerk-media-worker  target/release/bookclerk-jail
#           target/release/bookclerk-workerd
```

> [!IMPORTANT]
> The helpers aren't optional extras. Install them beside the host
> binaries: that is where they are looked for. By default a host refuses
> media work, or declines to load a plugin, rather than run either
> unconfined.
>
> - `bookclerk-media-worker` runs every decode, encode and packaging step
>   in a confined child process (see [Media worker](../server/media.md)).
> - `bookclerk-jail` starts each external plugin guest inside its own
>   jail (see [The guest jail](../internals/guest-jail.md)).
> - `bookclerk-workerd` is the front door every plugin is spawned
>   through, next to the pinned `workerd` binary (`cargo ensure-workerd`).

Or run from the workspace without installing:

```sh
export BOOKCLERK_FILES_DIR=./BookclerkFiles
cargo run -p bookclerk-cli -- version
```

## Initialize config

```sh
export BOOKCLERK_FILES_DIR=./BookclerkFiles
mkdir -p "$BOOKCLERK_FILES_DIR"
cp config/config.example.toml "$BOOKCLERK_FILES_DIR/config.toml"
# [database] and daemon settings only; plugins are configured in the database (below)
```

On first use, Bookclerk also creates `library.db`, `cache/`,
`search_index/` and `plugins/` under the files folder. Store credentials
are stored in the `encrypted_secrets` table inside `library.db`; there is
no `Accounts/` folder.

### Start from a fresh database

The first-party role model (Owner, Administrator, Member) is greenfield:
no migration upgrades a pre-Owner Administrator. After pulling this
schema, recreate a testing or development database rather than reusing an
old `library.db`:

```sh
cargo reset --yes   # wipes BookclerkFiles/ (not target/ or .cargo-home/)
```

Then set up the first Owner from Settings (an operator session), or with
`POST /api/auth/bootstrap`.

## Connect a store

Stores are connected in the **Accounts** screen of the web app (from a
claim ticket or an integration login, then Accounts → Connect). There is
no `bookclerk auth` command group, and the operator token can't link
bookstore sources.

### Audible (OAuth)

In Accounts, choose Audible and complete Amazon OAuth in the browser.

> [!NOTE]
> Amazon accounts with **2FA or MFA must complete the one-time code in the
> browser**.

Operators moving existing credentials can import an audible-rs auth file
through the Audible plugin's migrate path (see
[Backups](../server/backups.md) and [Stores](../server/stores.md)).

### Wrap the master key

To wrap `master.key` at rest (strongly recommended for production):

```sh
read -rsp 'Bookclerk auth password: ' BOOKCLERK_AUTH_PASSWORD
printf '\n'
export BOOKCLERK_AUTH_PASSWORD
```

### Password stores (Libro.fm, Chirp, GraphicAudio)

Connect from Accounts with your email and password. Passwords are never
put on the command line or read from the environment. Details for each
store are in [Stores](../server/stores.md).

### Destinations and plugin settings

```sh
bookclerk plugins list
bookclerk plugins config set local root /data/Audiobooks
bookclerk plugins config set s3 bucket my-audiobooks
bookclerk plugins deploy s3
```

See [Configuration → Plugin instances](../server/configuration.md#plugin-instances).

### Account hygiene

```sh
bookclerk library accounts
bookclerk library set-scan <account> --scan false   # exclude from bare/daemon scans
```

A bare `library scan` and daemon schedules honor `scan_enabled`. An
explicit `library scan --account <id>` bypasses that flag, for a one-shot
sync.

## Scan and acquire

```sh
bookclerk library scan                         # all enabled sources
bookclerk library scan --source audible        # one store
bookclerk library acquire --asin B0EXAMPLE     # Audible
bookclerk library acquire --isbn 978…          # Libro / ISBN attribute
bookclerk library search 'title:Dune'
```

A title id may be a UUID, ASIN, ISBN or source product id.

> [!NOTE]
> `library scan` and `library acquire` queue jobs that a running
> `bookclerkd` runs (`cargo dev` in development), and wait for them. Start
> the daemon first, or pass `--no-wait` to only queue. See
> [Jobs](../internals/jobs.md#planners-and-the-cli).

### Audiobooks already on disk or in S3

```sh
bookclerk library scan --match-storage
# optional: relocate matched files onto the naming-profile layout
bookclerk library scan --match-storage --fix-layout
bookclerk library acquire          # skips matched titles
bookclerk library acquire --force  # re-download anyway
```

Matching lists audio extensions and probes object metadata (without
downloading the body). Then it falls back to ASIN and ISBN tokens in the
path.

## Run the daemon

```sh
bookclerkd
# or: cargo run -p bookclerkd
```

Keep `library.auto_acquire = false` until you are comfortable with
scheduled downloads. For packaging, see
[Running Bookclerk](../server/operations.md).

## Next steps

- [Output and destinations](../server/output.md): tune naming and
  destinations.
- [Integrations](../server/integrations.md): connect Audiobookshelf.
- [Dynamic plugins](../plugins/README.md): install a third-party plugin.
- [Backups, updates, import and export](../server/backups.md): coming from
  Libation.
