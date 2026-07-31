# Getting started

## Prerequisites

- Rust stable (see `rust-toolchain.toml`; edition 2021, MSRV in workspace)
- Network access to the stores you intend to use
- Optional: Docker if you prefer the packaged daemon image

No `ffmpeg` or `aaxclean-cli` binaries are required. Decrypt, remux, MP3 encode,
metadata fix-up, and chapter split are native Rust.

## Build

```bash
cargo build --release -p bookclerk-cli -p bookclerkd -p bookclerk-media-worker
export PATH="$PWD/target/release:$PATH"
# binaries: target/release/bookclerk  target/release/bookclerkd
#           target/release/bookclerk-media-worker
```

`bookclerk-media-worker` runs every decode, encode, and packaging step in a
confined child process. Install it beside the other two binaries: that is where
both hosts look for it, and by default they refuse media work rather than run
codecs unconfined. See [media.md](media.md).

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

## Authenticate a store

### Audible (OAuth)

```bash
bookclerk auth login -m us
```

Default mode starts a local callback server and prints a URL + terminal QR
(good for SSH with port-forward). Use `--external` to paste a redirect URL, or
`--response-url` for scripts.

Amazon accounts with **2FA/MFA must complete OTP in the browser**. There is no
username/password CLI for Audible. Alternatives:

- Import an existing audible-rs auth file: `bookclerk auth import path/to/file.audible.auth`
- Migrate classic Libation accounts: see [migration.md](migration.md)

Wrap `master.key` at rest (strongly recommended for production):

```bash
export BOOKCLERK_AUTH_PASSWORD='your-strong-passphrase'
bookclerk auth login --force
```

### Password stores (Libro.fm, Chirp, GraphicAudio)

```bash
export BOOKCLERK_LIBRO_PASSWORD='…'
bookclerk auth login --source libro --email you@example.com

export BOOKCLERK_CHIRP_PASSWORD='…'
bookclerk auth login --source chirp --email you@example.com

export BOOKCLERK_GA_PASSWORD='…'
bookclerk auth login --source graphicaudio --email you@example.com
```

Passwords come from the env var or an interactive prompt — **never** on argv.
Details per store: [sources.md](sources.md).

### Account hygiene

```bash
bookclerk auth list
bookclerk auth set-scan <account> --scan false   # exclude from bare/daemon scans
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
