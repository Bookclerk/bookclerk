# Getting started

## Prerequisites

- Rust stable (see `rust-toolchain.toml`; edition 2021, MSRV in workspace)
- Network access to the stores you intend to use
- Optional: Docker if you prefer the packaged daemon image

No `ffmpeg` or `aaxclean-cli` binaries are required. Decrypt, remux, MP3 encode,
metadata fix-up, and chapter split are native Rust.

## Build

```bash
cargo build --release -p bookclerk-cli -p bookclerkd \
  -p bookclerk-media-worker -p bookclerk-jail -p bookclerk-workerd
export PATH="$PWD/target/release:$PATH"
# binaries: target/release/bookclerk  target/release/bookclerkd
#           target/release/bookclerk-media-worker  target/release/bookclerk-jail
#           target/release/bookclerk-workerd
```

The helpers are not optional extras. `bookclerk-media-worker` runs every
decode, encode, and packaging step in a confined child process
([media.md](media.md)); `bookclerk-jail` starts each external plugin guest inside
its own jail ([plugins.md](plugins.md#the-guest-jail)). Install both beside the
host binaries — that is where they are looked for, and by default a host refuses
media work or declines to load a plugin rather than run either unconfined.

Or run from the workspace without installing:

```bash
export BOOKCLERK_FILES_DIR=./BookclerkFiles
cargo run -p bookclerk-cli -- version
```

## Initialize config

```bash
export BOOKCLERK_FILES_DIR=./BookclerkFiles
mkdir -p "$BOOKCLERK_FILES_DIR"
cp config/config.example.toml "$BOOKCLERK_FILES_DIR/config.toml"
# edit sources, output.local.root, integrations as needed
```

On first use Bookclerk also creates `library.db`, `cache/`, `search_index/`,
and `plugins/` under the files directory. Auth credentials are stored in the
`encrypted_secrets` table inside `library.db` — there is no `Accounts/` directory.

The first-party role model (Owner / Administrator / Member) is greenfield: there
is no migration that upgrades a pre-Owner Administrator. After pulling this
schema, recreate a testing/dev database rather than reusing an old
`library.db`:

```bash
cargo reset --yes   # wipes BookclerkFiles/ (not target/ or .cargo-home/)
```

Then bootstrap the first Owner from Settings (operator session) or
`POST /api/auth/bootstrap`.

## Authenticate a store

Store connect lives in the **User SPA Accounts** UI (claim ticket or integration
login → Accounts → Connect). There is no `bookclerk auth` CLI group; the operator
token cannot link bookstore sources.

### Audible (OAuth)

In Accounts, choose Audible and complete Amazon OAuth in the browser. Amazon
accounts with **2FA/MFA must complete OTP in the browser**.

Alternatives for operators migrating credentials:

- Import an existing audible-rs auth file via the Audible plugin / migrate path
  (see [migration.md](migration.md) and [sources.md](sources.md))

Wrap `master.key` at rest (strongly recommended for production):

```bash
export BOOKCLERK_AUTH_PASSWORD='your-strong-passphrase'
```

### Password stores (Libro.fm, Chirp, GraphicAudio)

Connect from Accounts with email + password (passwords are never placed on CLI
argv). Details per store: [sources.md](sources.md).

### Account hygiene

```bash
bookclerk library accounts
bookclerk library set-scan <account> --scan false   # exclude from bare/daemon scans
```

Bare `library scan` and daemon schedules honor `scan_enabled`. Explicit
`library scan --account <id>` bypasses that flag for one-shot syncs.

## Scan and acquire

```bash
bookclerk library scan                         # all enabled sources
bookclerk library scan --source audible        # one store
bookclerk library acquire --asin B0EXAMPLE     # Audible
bookclerk library acquire --isbn 978…          # Libro / ISBN attribute
bookclerk library search 'title:Dune'
```

Title ids may be UUID, ASIN, ISBN, or source product id.

### Existing audiobooks already on disk/S3

```bash
bookclerk library scan --match-storage
# optional: relocate matched files onto the naming-profile layout
bookclerk library scan --match-storage --fix-layout
bookclerk library acquire          # skips matched titles
bookclerk library acquire --force  # re-download anyway
```

Matching lists audio extensions, probes object metadata (no body download), then
falls back to ASIN/ISBN tokens in the path.

## Run the daemon

```bash
bookclerkd
# or: cargo run -p bookclerkd
```

Keep `library.auto_acquire = false` until you are comfortable with scheduled
downloads. Packaging notes: [operations.md](operations.md).

## Next steps

- Tune naming and destinations — [destinations.md](destinations.md)
- Wire Audiobookshelf — [integrations.md](integrations.md)
- Install a third-party plugin — [plugins.md](plugins.md)
- Coming from Libation — [migration.md](migration.md)
