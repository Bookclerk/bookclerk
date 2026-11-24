# CLI reference

Every built-in command and option of the `bookclerk` command-line tool. During
development, run it as `cargo dev-cli -- <command>` or
`cargo run -p bookclerk-cli -- <command>`. `bookclerk <command> --help`
prints the same text in a terminal.

Installed plugins can add their own commands under
`bookclerk plugins <plugin-id>`. Those depend on which plugins are installed,
so they are not listed here; `bookclerk plugins <plugin-id> --help` shows
them.

This page is generated from the CLI's own definitions, so do not edit it by
hand. After changing a command, run
`UPDATE_CLI_REFERENCE=1 cargo test -p bookclerk-cli cli_reference` and commit
the result.

## Global options

These options work with every command. Some commands have their own option of the same name: written after the command's name, it means the command's option, and written before it, the global one. These are `bookclerk export postgres --format`.

- `--bookclerk-files, --bookclerkFiles <BOOKCLERK_FILES>`: Bookclerk files directory (`BOOKCLERK_FILES_DIR`) (environment variable `BOOKCLERK_FILES_DIR`)
- `--config <CONFIG>`: Path to config.toml (default: `{bookclerk-files}/config.toml`) (environment variable `BOOKCLERK_CONFIG`)
- `-v, --verbose`: Increase tracing verbosity (`-v` warnings, `-vv` debug, `-vvv` trace). Default is quiet: command output only (override with `BOOKCLERK_LOG`)
- `--format <FORMAT>`: Machine-readable output for list/status style commands (one of `text`, `json`; default `text`)

## Commands

- [`bookclerk library`](#bookclerk-library): Library scan, acquire, search, accounts, and status
- [`bookclerk discover`](#bookclerk-discover): Recommendations, embeddings, listening sync, and title requests
- [`bookclerk integrations`](#bookclerk-integrations): Outbound integrations (Audiobookshelf) and claim tickets
- [`bookclerk plugins`](#bookclerk-plugins): Dynamically discovered third-party plugins
- [`bookclerk config`](#bookclerk-config): Read or write configuration values
- [`bookclerk export`](#bookclerk-export): Export library data, backups, or Libation-compatible files
- [`bookclerk import`](#bookclerk-import): Import native backups or classic Libation Files
- [`bookclerk db`](#bookclerk-db): Host schema version, backups, and explicit migrate
- [`bookclerk daemon`](#bookclerk-daemon): Talk to a running bookclerkd control plane
- [`bookclerk events`](#bookclerk-events): Durable domain-event outbox and deliveries
- [`bookclerk libraries`](#bookclerk-libraries): Owner libraries this install hosts for, and consent to their plugins
- [`bookclerk ingress`](#bookclerk-ingress): The library's public address from an ingress plugin
- [`bookclerk nodes`](#bookclerk-nodes): Library nodes: liveness, capacity, budgets, and placement labels
- [`bookclerk users`](#bookclerk-users): The library's people: patron scopes, member controls and Administrator switches
- [`bookclerk queues`](#bookclerk-queues): Plugin-owned queues on the outbox: counters, consumers, messages
- [`bookclerk media`](#bookclerk-media): What this build's media worker reads and writes
- [`bookclerk login`](#bookclerk-login): Print a loopback operator sign-in URL (requires a running daemon)
- [`bookclerk diagnostics`](#bookclerk-diagnostics): Diagnostics ring buffer and opt-in upload
- [`bookclerk version`](#bookclerk-version): Print version information

## `bookclerk library`

Library scan, acquire, search, accounts, and status

```text
bookclerk library [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk library accounts`](#bookclerk-library-accounts): List configured bookstore accounts
- [`bookclerk library set-scan`](#bookclerk-library-set-scan): Enable or disable an account for library scans (GUI: Include in library scan)
- [`bookclerk library set-pace`](#bookclerk-library-set-pace): Show or set how fast Bookclerk downloads from an account when it starts the downloads itself (auto-acquire, upgrades, requests from other libraries). With no option, prints the account's pace
- [`bookclerk library status`](#bookclerk-library-status): Show token validity / refresh health across sources
- [`bookclerk library revoke-account`](#bookclerk-library-revoke-account): Remove store credentials but keep acquired books and account rows
- [`bookclerk library scan`](#bookclerk-library-scan): Sync the library from the content sources: queues one scan job per account (every scan-enabled account when none is named) for a bookclerkd node to run, and waits for them
- [`bookclerk library acquire`](#bookclerk-library-acquire): Download + decrypt + store titles: queues one acquire job per title id and account it is in (or one for every pending title) for a bookclerkd node to run, and waits for them. `--dry-run` only prints planned keys, here
- [`bookclerk library upload`](#bookclerk-library-upload): Upload files, folders or ZIP / tar archives to a running bookclerkd as one upload, then wait for the ingest job
- [`bookclerk library set-status`](#bookclerk-library-set-status): Match storage files to library rows and update acquire status
- [`bookclerk library get-license`](#bookclerk-library-get-license): Fetch a content license for an ASIN
- [`bookclerk library search`](#bookclerk-library-search): Search the library's works by title, people, series or store id
- [`bookclerk library export`](#bookclerk-library-export): Export library titles as CSV / JSON / XLSX (alias of `export library`): the whole library, or with `--user` that person's view
- [`bookclerk library list`](#bookclerk-library-list): List books in the local library DB
- [`bookclerk library convert`](#bookclerk-library-convert): Add another copy of stored books, made from the copy the library holds and kept beside it (queued for the daemon, like acquire)
- [`bookclerk library filters`](#bookclerk-library-filters): Manage saved quick filters
- [`bookclerk library tags`](#bookclerk-library-tags): Read or set a work's library tags, which everyone sees, the Library filters by, and a patron scope can include or exclude
- [`bookclerk library collections`](#bookclerk-library-collections): Read or set the library's collections: named parts of the library, each defined by storage key prefixes, which the Library filters by and a patron scope can include or exclude

### `bookclerk library accounts`

List configured bookstore accounts

```text
bookclerk library accounts [OPTIONS]
```

Options:

- `--source <SOURCE>`: Content source filter. Omit for all
- `-b, --bare`: Tab-separated values for scripts (source, account, name, locale, scan, auth)

### `bookclerk library set-scan`

Enable or disable an account for library scans (GUI: Include in library scan)

```text
bookclerk library set-scan [OPTIONS] <ACCOUNT>
```

Arguments:

- `<ACCOUNT>`: Account id, auth-file stem, or nickname

Options:

- `--source <SOURCE>`: Content source, when the same account id exists on several plugins
- `--scan <SCAN>`: Include this account when scanning (default: true) (one of `true`, `false`)

### `bookclerk library set-pace`

Show or set how fast Bookclerk downloads from an account when it starts the downloads itself (auto-acquire, upgrades, requests from other libraries). With no option, prints the account's pace

```text
bookclerk library set-pace [OPTIONS] <ACCOUNT>
```

Arguments:

- `<ACCOUNT>`: Account id, auth-file stem, or nickname

Options:

- `--source <SOURCE>`: Content source, when the same account id exists on several plugins
- `--per-day <PER_DAY>`: Downloads allowed in any rolling 24 hours (1 to 10000)
- `--gap-seconds <GAP_SECONDS>`: Seconds between two downloads (0 to 86400)
- `--unlimited`: No daily budget and no gap (a store's cool-down still applies)
- `--default`: Go back to the storefront's declared pace
- `--clear-cooldown`: End the cool-down the store asked for

### `bookclerk library status`

Show token validity / refresh health across sources

```text
bookclerk library status [OPTIONS]
```

Options:

- `--source <SOURCE>`: Content source filter. Omit for all

### `bookclerk library revoke-account`

Remove store credentials but keep acquired books and account rows

```text
bookclerk library revoke-account [OPTIONS] <ACCOUNT>
```

Arguments:

- `<ACCOUNT>`: Account id, auth-file stem, or nickname

Options:

- `--source <SOURCE>`: Content source, when the same account id exists on several plugins

### `bookclerk library scan`

Sync the library from the content sources: queues one scan job per account (every scan-enabled account when none is named) for a bookclerkd node to run, and waits for them

```text
bookclerk library scan [OPTIONS] [ACCOUNT]...
```

Arguments:

- `[ACCOUNT]...`: Account nickname(s) or id(s) to scan

Options:

- `--account <ACCOUNT>`: Limit sync to one account (alias for positional account list)
- `--source <SOURCE>`: Limit to one content source (`audible`, `libro`, `graphicaudio`, or `chirp`). Default: all
- `--instance <INSTANCE>`: Pin the scan to one deployed plugin instance (default: each plugin's default instance)
- `--match-storage`: After scan, match existing files in storage to library rows. Lists `.m4b` / `.mp3` / `.m4a` / `.flac` / `.aac` / `.ogg` / `.oga`, probes object metadata (no body download), and falls back to ASIN/ISBN tokens in the path.
- `--fix-layout`: With `--match-storage`, relocate matched audio + sidecars onto the configured naming-profile layout (also `library.fix_storage_layout`)
- `--no-wait`: Queue the scan jobs and print their ids without waiting for a bookclerkd node to run them

### `bookclerk library acquire`

Download + decrypt + store titles: queues one acquire job per title id and account it is in (or one for every pending title) for a bookclerkd node to run, and waits for them. `--dry-run` only prints planned keys, here

```text
bookclerk library acquire [OPTIONS] [ID]...
```

Arguments:

- `[ID]...`: Positional title id(s): UUID, ASIN, ISBN, or product id

Options:

- `--asin <ASIN>`: Acquire a single ASIN / product id / UUID
- `--isbn <ISBN>`: Alias for `--asin` (Libro ISBN or any title id)
- `--account <ACCOUNT>`: Account id (required with `--asin` when multiple accounts exist)
- `--instance <INSTANCE>`: Fetch through this deployed plugin instance (default: the book's plugin default instance)
- `--dry-run`: Dry-run: print planned storage keys only
- `-f, --force`: Re-download even when matching media already exists in storage
- `-p, --pdf`: Download companion PDF only
- `--no-wait`: Queue the acquire jobs and print their ids without waiting for a bookclerkd node to run them

### `bookclerk library upload`

Upload files, folders or ZIP / tar archives to a running bookclerkd as one upload, then wait for the ingest job.

Each folder keeps its name and layout; symbolic links are skipped. The books are held by the library's first active Owner. Unidentified books are stored for an Administrator to review.

```text
bookclerk library upload [OPTIONS] <PATH>...
```

Arguments:

- `<PATH>...`: Files, folders and archives to upload

Options:

- `--no-wait`: Commit the upload and print its job id without waiting for the ingest job
- `--resume <UPLOAD_ID>`: Continue an earlier upload of the same paths that did not finish, from the bytes the daemon already holds

### `bookclerk library set-status`

Match storage files to library rows and update acquire status.

Lists audio in storage and probes custom object metadata (S3 `HeadObject` / local `.bookclerk-meta.json`) without downloading bodies. Falls back to ASIN/ISBN tokens embedded in the object key.

```text
bookclerk library set-status [OPTIONS] [ASIN]...
```

Arguments:

- `[ASIN]...`: Title ids to update (UUID, ASIN, ISBN, or product id); empty means all matching the account filter

Options:

- `--account <ACCOUNT>`: Limit matching to this account id, stem, or nickname
- `--keep-missing`: Do not clear Acquired status when the file is missing
- `--downloaded`: Only mark titles with matching files as Acquired
- `--not-downloaded`: Only clear Acquired status when the file is missing
- `-f, --force`: Force status without checking storage
- `--fix-layout`: Relocate matched audio + accompanying sidecars onto the configured naming-profile layout (`library.fix_storage_layout`)

### `bookclerk library get-license`

Fetch a content license for an ASIN

```text
bookclerk library get-license [OPTIONS] <ASIN>
```

Arguments:

- `<ASIN>`: Title id to resolve to an Audible ASIN (UUID, ASIN, ISBN, or product id)

Options:

- `--account <ACCOUNT>`: Audible account when the title is missing or owned on multiple accounts
- `--json`: Emit full license summary as JSON
- `--full`: Emit full license API response JSON (not summary)

### `bookclerk library search`

Search the library's works by title, people, series or store id

```text
bookclerk library search [OPTIONS] <QUERY>
```

Arguments:

- `<QUERY>`: Lucene-style query string

Options:

- `-n, --limit <LIMIT>`: Max results (0 = all) (default `10`)
- `-b, --bare`: Print work ids only
- `--rebuild-index`: Rebuild the works search index before querying
- `--filter <FILTER>`: Run a saved quick filter by name

### `bookclerk library export`

Export library titles as CSV / JSON / XLSX (alias of `export library`): the whole library, or with `--user` that person's view

```text
bookclerk library export [OPTIONS] --path <PATH> [ASINS]...
```

Arguments:

- `[ASINS]...`: Limit to specific ASINs

Options:

- `-p, --path <PATH>`: Output file path (required)
- `--csv`: Write CSV (default when no format flag is set and the path ends in `.csv`)
- `--json`: Write JSON
- `--xlsx`: Write XLSX
- `--account <ACCOUNT>`: Limit the export to this account id, stem, or nickname
- `--user <USER>`: Fill `tags` and `is_finished` with this person's own state (login name or user id); without it they are empty

### `bookclerk library list`

List books in the local library DB

```text
bookclerk library list [OPTIONS]
```

Options:

- `--account <ACCOUNT>`: Limit listed rows to this account id, stem, or nickname
- `--status <STATUS>`: Filter by acquire status (`acquired`, `not_downloaded`, …)

### `bookclerk library convert`

Add another copy of stored books, made from the copy the library holds and kept beside it (queued for the daemon, like acquire)

```text
bookclerk library convert [OPTIONS] <ID>...
```

Arguments:

- `<ID>...`: Title ids (uuid, ASIN, ISBN, product id) of the books to convert

Options:

- `--account <ACCOUNT>`: Limit conversion to this account id, stem, or nickname
- `--to <FORMAT:QUALITY>`: The copy to add, as `output.copies` writes it (`mp3:64k-mono`, `m4b:original`, `opus:32k`). Defaults to an MP3 at the library's `output.lame` quality
- `-f, --force`: Make the copy again when the book already has it
- `--no-wait`: Queue the jobs and print their ids without waiting

### `bookclerk library filters`

Manage saved quick filters

```text
bookclerk library filters [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk library filters list`](#bookclerk-library-filters-list): List saved filters
- [`bookclerk library filters save`](#bookclerk-library-filters-save): Save or update a named filter
- [`bookclerk library filters delete`](#bookclerk-library-filters-delete): Delete a saved filter

#### `bookclerk library filters list`

List saved filters

```text
bookclerk library filters list [OPTIONS]
```

#### `bookclerk library filters save`

Save or update a named filter

```text
bookclerk library filters save [OPTIONS] <NAME> <QUERY>
```

Arguments:

- `<NAME>`: Saved filter name to create or overwrite
- `<QUERY>`: Lucene-style query string

#### `bookclerk library filters delete`

Delete a saved filter

```text
bookclerk library filters delete [OPTIONS] <NAME>
```

Arguments:

- `<NAME>`: Saved filter name to delete

### `bookclerk library tags`

Read or set a work's library tags, which everyone sees, the Library filters by, and a patron scope can include or exclude

```text
bookclerk library tags [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk library tags get`](#bookclerk-library-tags-get): Print a work's library tags, one per line, with where each came from
- [`bookclerk library tags set`](#bookclerk-library-tags-set): Replace the tags set in Bookclerk on a work; tags read from a sidecar file stay

#### `bookclerk library tags get`

Print a work's library tags, one per line, with where each came from

```text
bookclerk library tags get [OPTIONS] <WORK>
```

Arguments:

- `<WORK>`: The work id (a top-level work, as `GET /api/library/works` lists it)

#### `bookclerk library tags set`

Replace the tags set in Bookclerk on a work; tags read from a sidecar file stay

```text
bookclerk library tags set [OPTIONS] <WORK>
```

Arguments:

- `<WORK>`: The work id

Options:

- `--tag <TAG>`: A tag to set (repeatable). With none, the work's tags set in Bookclerk are removed

### `bookclerk library collections`

Read or set the library's collections: named parts of the library, each defined by storage key prefixes, which the Library filters by and a patron scope can include or exclude

```text
bookclerk library collections [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk library collections get`](#bookclerk-library-collections-get): Print each collection with its prefixes
- [`bookclerk library collections set`](#bookclerk-library-collections-set): Replace every collection. The page index is rebuilt by a running daemon's next heartbeat

#### `bookclerk library collections get`

Print each collection with its prefixes

```text
bookclerk library collections get [OPTIONS]
```

#### `bookclerk library collections set`

Replace every collection. The page index is rebuilt by a running daemon's next heartbeat

```text
bookclerk library collections set [OPTIONS]
```

Options:

- `--collection <NAME=PREFIXES>`: A collection as `NAME=PREFIX[,PREFIX…]` (`Kids=kids/,children/`), repeatable. With none, every collection is removed

## `bookclerk discover`

Recommendations, embeddings, listening sync, and title requests

```text
bookclerk discover [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk discover recommend`](#bookclerk-discover-recommend): Optionally enrich + embed works, then print recommendations
- [`bookclerk discover enrich-metadata`](#bookclerk-discover-enrich-metadata): Fill metadata gaps (description, subjects, cover, language) from the deployed metadata-provider plugins, such as Open Library
- [`bookclerk discover embed`](#bookclerk-discover-embed): Embed dirty works with the built-in word-hash vectors
- [`bookclerk discover sync-listening`](#bookclerk-discover-sync-listening): Sync listening progress from all capable integrations into the library DB
- [`bookclerk discover wishlist`](#bookclerk-discover-wishlist): Personal wishlist helpers (also feed the shared global queue)

### `bookclerk discover recommend`

Optionally enrich + embed works, then print recommendations

```text
bookclerk discover recommend [OPTIONS]
```

Options:

- `-n, --limit <LIMIT>`: Max recommendations to print
- `--no-purchase-hints`: Skip purchase-hint HTTP lookups
- `--user <USER>`: Filter listening signals to one external user id (any listening integration)
- `--no-listening`: Ignore listening_progress entirely (owned-library taste only)
- `--listening-provider <ID>`: Only use listening rows from these integration ids (repeatable)

### `bookclerk discover enrich-metadata`

Fill metadata gaps (description, subjects, cover, language) from the deployed metadata-provider plugins, such as Open Library

```text
bookclerk discover enrich-metadata [OPTIONS]
```

### `bookclerk discover embed`

Embed dirty works with the built-in word-hash vectors

```text
bookclerk discover embed [OPTIONS]
```

### `bookclerk discover sync-listening`

Sync listening progress from all capable integrations into the library DB

```text
bookclerk discover sync-listening [OPTIONS]
```

### `bookclerk discover wishlist`

Personal wishlist helpers (also feed the shared global queue)

```text
bookclerk discover wishlist [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk discover wishlist add`](#bookclerk-discover-wishlist-add): Add an open wishlist item
- [`bookclerk discover wishlist list`](#bookclerk-discover-wishlist-list): List open wishlist items (operator-owned when no portal identity)
- [`bookclerk discover wishlist remove`](#bookclerk-discover-wishlist-remove): Un-wishlist by uuid (sets status to cancelled)

#### `bookclerk discover wishlist add`

Add an open wishlist item

```text
bookclerk discover wishlist add [OPTIONS] <TITLE>
```

Arguments:

- `<TITLE>`: Display title for the wishlist row (required positional)

Options:

- `--authors <AUTHORS>`: Optional author list stored on the request
- `--asin <ASIN>`: Optional Audible ASIN used for identity matching
- `--isbn <ISBN>`: Optional ISBN-13 used for identity matching
- `--notes <NOTES>`: Optional operator notes stored on the request

#### `bookclerk discover wishlist list`

List open wishlist items (operator-owned when no portal identity)

```text
bookclerk discover wishlist list [OPTIONS]
```

#### `bookclerk discover wishlist remove`

Un-wishlist by uuid (sets status to cancelled)

```text
bookclerk discover wishlist remove [OPTIONS] <UUID>
```

Arguments:

- `<UUID>`: Wishlist entry UUID to cancel

## `bookclerk integrations`

Outbound integrations (Audiobookshelf) and claim tickets

```text
bookclerk integrations [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk integrations status`](#bookclerk-integrations-status): Show health of configured integrations
- [`bookclerk integrations test`](#bookclerk-integrations-test): Probe connectivity for one (or all) enabled integrations
- [`bookclerk integrations tickets`](#bookclerk-integrations-tickets): Claim ticket management
- [`bookclerk integrations scan`](#bookclerk-integrations-scan): Trigger a remote library scan on an integration that supports it

### `bookclerk integrations status`

Show health of configured integrations

```text
bookclerk integrations status [OPTIONS]
```

### `bookclerk integrations test`

Probe connectivity for one (or all) enabled integrations

```text
bookclerk integrations test [OPTIONS]
```

Options:

- `--integration <INTEGRATION>`: Integration id (default: all enabled)

### `bookclerk integrations tickets`

Claim ticket management

```text
bookclerk integrations tickets [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk integrations tickets create`](#bookclerk-integrations-tickets-create): Mint a claim ticket for an external identity
- [`bookclerk integrations tickets list`](#bookclerk-integrations-tickets-list): List open (unredeemed, unexpired) claim tickets

#### `bookclerk integrations tickets create`

Mint a claim ticket for an external identity

```text
bookclerk integrations tickets create [OPTIONS] --external-user-id <EXTERNAL_USER_ID>
```

Options:

- `--provider <PROVIDER>`: Integration provider id (`audiobookshelf`) (default `audiobookshelf`)
- `--external-user-id <EXTERNAL_USER_ID>`: External user id from the provider (required)
- `--label <LABEL>`: Optional display label

#### `bookclerk integrations tickets list`

List open (unredeemed, unexpired) claim tickets

```text
bookclerk integrations tickets list [OPTIONS]
```

### `bookclerk integrations scan`

Trigger a remote library scan on an integration that supports it

```text
bookclerk integrations scan [OPTIONS] --integration <INTEGRATION>
```

Options:

- `--integration <INTEGRATION>`: Integration id (`audiobookshelf`, …) (required)
- `--force`: Force full rescan when the integration supports it

## `bookclerk plugins`

Dynamically discovered third-party plugins

```text
bookclerk plugins [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins list`](#bookclerk-plugins-list): List plugins found under plugin search directories
- [`bookclerk plugins search`](#bookclerk-plugins-search): Search configured registries (and crates.io) for Bookclerk plugins
- [`bookclerk plugins install`](#bookclerk-plugins-install): Install a plugin version into the library from a source-qualified coordinate or a local archive. The first installed version (or `--activate`) becomes the active version every node runs; deploy it with `plugins deploy`
- [`bookclerk plugins update`](#bookclerk-plugins-update): Install and activate a newer version of an installed plugin (or update a staged package by its receipt)
- [`bookclerk plugins rollback`](#bookclerk-plugins-rollback): Activate an earlier installed version of a plugin
- [`bookclerk plugins versions`](#bookclerk-plugins-versions): List installed versions (the active one is starred)
- [`bookclerk plugins uninstall`](#bookclerk-plugins-uninstall): Uninstall a plugin: clear its active version and undeploy it from every node (or remove a staged package directory)
- [`bookclerk plugins info`](#bookclerk-plugins-info): Show details for one discovered plugin (local + receipt)
- [`bookclerk plugins diagnose`](#bookclerk-plugins-diagnose): Run diagnose probes for one (or all) plugins that support it
- [`bookclerk plugins doctor`](#bookclerk-plugins-doctor): Check install integrity, target, jail, and describe/health status
- [`bookclerk plugins deploy`](#bookclerk-plugins-deploy): Deploy a plugin instance. With no flags it runs on this node (or on `--node`) by hand: the instance's deployment there is `present`. With `--replicas` the library scheduler (one daemon, under a library lease) places it on that many nodes carrying every `--on` label and writes the deployments itself; `--manual` hands those deployments back to be managed by hand
- [`bookclerk plugins undeploy`](#bookclerk-plugins-undeploy): Undeploy a plugin instance from this node (or `--node`): its deployment there becomes `absent` and the guest is retired
- [`bookclerk plugins instances`](#bookclerk-plugins-instances): Plugin instances: one per plugin by default, more when an operator wants several configurations of one plugin
- [`bookclerk plugins config`](#bookclerk-plugins-config): Instance settings, validated against the plugin's declared schema
- [`bookclerk plugins approve`](#bookclerk-plugins-approve): Approve network domains and host bindings for a plugin
- [`bookclerk plugins registry`](#bookclerk-plugins-registry): Manage configured plugin registries
- [`bookclerk plugins db`](#bookclerk-plugins-db): Manage isolated plugin database bindings (`capabilities.bindings.databases`)

### `bookclerk plugins list`

List plugins found under plugin search directories

```text
bookclerk plugins list [OPTIONS]
```

### `bookclerk plugins search`

Search configured registries (and crates.io) for Bookclerk plugins

```text
bookclerk plugins search [OPTIONS] [QUERY]
```

Arguments:

- `[QUERY]`: Free-text query

Options:

- `--limit <LIMIT>`: Max hits to fetch (1–100) (default `25`)

### `bookclerk plugins install`

Install a plugin version into the library from a source-qualified coordinate or a local archive. The first installed version (or `--activate`) becomes the active version every node runs; deploy it with `plugins deploy`

```text
bookclerk plugins install [OPTIONS] <COORDINATE>
```

Arguments:

- `<COORDINATE>`: Coordinate (`cargo:…@ver`, `registry:…#name@ver`, `local:…`) or legacy crate name

Options:

- `--archive <ARCHIVE>`: Local archive path (sets artifact to this file; use with a static manifest coord)
- `--manifest <MANIFEST>`: Path to a Bookclerk package manifest JSON (required with `--archive` alone)
- `--target <TARGET>`: Override Bookclerk target (e.g. `linux-x64-gnu`)
- `--activate`: Make this the active version even when another version is active
- `--approve-capabilities`: Record the requested consent as approved (no separate `approve`)
- `--allow-unverified-publisher`: Allow community packages without independent publisher authenticity (digest still required)
- `--dry-run`: Do not download; only resolve and print the plan
- `--offline`: Refuse remote downloads

### `bookclerk plugins update`

Install and activate a newer version of an installed plugin (or update a staged package by its receipt)

```text
bookclerk plugins update [OPTIONS] [ID]
```

Arguments:

- `[ID]`: Plugin runtime id (default: all with receipts)

Options:

- `--to <TO>`: Exact version to install
- `--allow-unverified-publisher`: Allow community packages without independent publisher authenticity on this update (digest still required)
- `--approve-capabilities`: Approve sandbox/network capability changes without a separate prompt
- `--dry-run`: Resolve and print the plan without downloading

### `bookclerk plugins rollback`

Activate an earlier installed version of a plugin

```text
bookclerk plugins rollback [OPTIONS] <ID> <VERSION>
```

Arguments:

- `<ID>`: Plugin alias or PluginKey
- `<VERSION>`: Installed version to activate

### `bookclerk plugins versions`

List installed versions (the active one is starred)

```text
bookclerk plugins versions [OPTIONS] [ID]
```

Arguments:

- `[ID]`: Only this plugin (alias or PluginKey)

### `bookclerk plugins uninstall`

Uninstall a plugin: clear its active version and undeploy it from every node (or remove a staged package directory)

```text
bookclerk plugins uninstall [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Plugin runtime id

Options:

- `--purge-state`: Also delete `$FILES_DIR/plugin-state/<PluginKey>/`
- `--discard-acl-journals`: Delete rejected ACL journals with plugin state. Requires `--purge-state`. Does not revoke the package-SID ACEs those records name; read the rejected file and remove those ACEs by hand first

### `bookclerk plugins info`

Show details for one discovered plugin (local + receipt)

```text
bookclerk plugins info [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Plugin id

### `bookclerk plugins diagnose`

Run diagnose probes for one (or all) plugins that support it

```text
bookclerk plugins diagnose [OPTIONS] [ID]
```

Arguments:

- `[ID]`: Plugin id (default: all discovered that are enabled)

### `bookclerk plugins doctor`

Check install integrity, target, jail, and describe/health status

```text
bookclerk plugins doctor [OPTIONS] [ID]
```

Arguments:

- `[ID]`: Plugin id (default: all discovered)

### `bookclerk plugins deploy`

Deploy a plugin instance. With no flags it runs on this node (or on `--node`) by hand: the instance's deployment there is `present`. With `--replicas` the library scheduler (one daemon, under a library lease) places it on that many nodes carrying every `--on` label and writes the deployments itself; `--manual` hands those deployments back to be managed by hand.

Database adapters are the bootstrap exception and still select `[database].plugin` in `config.toml`.

```text
bookclerk plugins deploy [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Plugin alias, PluginKey, or plugin instance id (an alias or key names the plugin's default instance)

Options:

- `--node <NODE>`: Node to deploy on by hand (default: this node)
- `--replicas <REPLICAS>`: Nodes that should run the instance: a number (0 stops every replica) or `all` (every live, schedulable node with room)
- `--on <LABEL>`: Label constraint every chosen node must satisfy (`nas`, `zone=eu`, `node=<id>`, `target=linux-x64-gnu`); prefix with `!` to exclude. Repeatable
- `--manual`: Drop the scheduler placement: the instance's deployments stay as they are and are managed by hand again

### `bookclerk plugins undeploy`

Undeploy a plugin instance from this node (or `--node`): its deployment there becomes `absent` and the guest is retired

```text
bookclerk plugins undeploy [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Plugin alias, PluginKey, or plugin instance id

Options:

- `--node <NODE>`: Node to undeploy from (default: this node)

### `bookclerk plugins instances`

Plugin instances: one per plugin by default, more when an operator wants several configurations of one plugin

```text
bookclerk plugins instances [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins instances list`](#bookclerk-plugins-instances-list): List instances with their deployments on every node
- [`bookclerk plugins instances create`](#bookclerk-plugins-instances-create): Create another instance of a plugin that has a pinned schema
- [`bookclerk plugins instances delete`](#bookclerk-plugins-instances-delete): Delete an instance that has no deployment on any node
- [`bookclerk plugins instances filter`](#bookclerk-plugins-instances-filter): Show or change which books a storage destination takes
- [`bookclerk plugins instances intake`](#bookclerk-plugins-instances-intake): Show or change whether a storage instance is a drop folder: a folder Bookclerk watches and adds whatever lands there to the library

#### `bookclerk plugins instances list`

List instances with their deployments on every node

```text
bookclerk plugins instances list [OPTIONS]
```

Options:

- `--plugin <PLUGIN>`: Only instances of this plugin (alias or PluginKey)

#### `bookclerk plugins instances create`

Create another instance of a plugin that has a pinned schema

```text
bookclerk plugins instances create [OPTIONS] <PLUGIN>
```

Arguments:

- `<PLUGIN>`: Plugin alias or PluginKey

Options:

- `--name <NAME>`: Operator-facing name
- `--node <NODE_ID>`: Make the instance local to this node (a node's own `local` output, for example): it deploys only there and is never placed by the scheduler

#### `bookclerk plugins instances delete`

Delete an instance that has no deployment on any node

```text
bookclerk plugins instances delete [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin instance id

#### `bookclerk plugins instances filter`

Show or change which books a storage destination takes

```text
bookclerk plugins instances filter [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins instances filter get`](#bookclerk-plugins-instances-filter-get): Show a destination's filter
- [`bookclerk plugins instances filter set`](#bookclerk-plugins-instances-filter-set): Replace a destination's filter. Each rule is `FIELD=VALUE`, where FIELD is `patron` (a user id), `storefront` (a plugin alias such as `audible`), `account` (a store account id), `media_kind` (`book` or `podcast`), `format` (a copy format: `m4b`, `mp3`, `mp3_chapters` or `original`) or `quality` (a copy quality such as `original`, `64k-mono` or `v2`). With any `--include`, the destination takes only books that match one; an `--exclude` always wins. `format` and `quality` rules choose which of the library's copies (`output.copies`) the destination receives, the same way among themselves. Acquires that start afterwards use the new filter
- [`bookclerk plugins instances filter clear`](#bookclerk-plugins-instances-filter-clear): Remove a destination's filter so it takes every book

##### `bookclerk plugins instances filter get`

Show a destination's filter

```text
bookclerk plugins instances filter get [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the destination

##### `bookclerk plugins instances filter set`

Replace a destination's filter. Each rule is `FIELD=VALUE`, where FIELD is `patron` (a user id), `storefront` (a plugin alias such as `audible`), `account` (a store account id), `media_kind` (`book` or `podcast`), `format` (a copy format: `m4b`, `mp3`, `mp3_chapters` or `original`) or `quality` (a copy quality such as `original`, `64k-mono` or `v2`). With any `--include`, the destination takes only books that match one; an `--exclude` always wins. `format` and `quality` rules choose which of the library's copies (`output.copies`) the destination receives, the same way among themselves. Acquires that start afterwards use the new filter

```text
bookclerk plugins instances filter set [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the destination

Options:

- `--include <FIELD=VALUE>`: Take books that match this rule (repeatable)
- `--exclude <FIELD=VALUE>`: Refuse books that match this rule (repeatable)

##### `bookclerk plugins instances filter clear`

Remove a destination's filter so it takes every book

```text
bookclerk plugins instances filter clear [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the destination

#### `bookclerk plugins instances intake`

Show or change whether a storage instance is a drop folder: a folder Bookclerk watches and adds whatever lands there to the library

```text
bookclerk plugins instances intake [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins instances intake get`](#bookclerk-plugins-instances-intake-get): Show whether a storage instance is a drop folder, and its settings
- [`bookclerk plugins instances intake set`](#bookclerk-plugins-instances-intake-set): Make a storage instance a drop folder, or change a drop folder's settings. Options left out keep their current value (or the default, for a new drop folder). Turning a destination into a drop folder needs the instance undeployed on every node
- [`bookclerk plugins instances intake clear`](#bookclerk-plugins-instances-intake-clear): Make a drop folder a destination again. Needs the instance undeployed on every node

##### `bookclerk plugins instances intake get`

Show whether a storage instance is a drop folder, and its settings

```text
bookclerk plugins instances intake get [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the storage instance

##### `bookclerk plugins instances intake set`

Make a storage instance a drop folder, or change a drop folder's settings. Options left out keep their current value (or the default, for a new drop folder). Turning a destination into a drop folder needs the instance undeployed on every node

```text
bookclerk plugins instances intake set [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the storage instance

Options:

- `--patron <USER_ID>`: User id of the patron who holds every book the folder adds
- `--subfolder`: Each top-level folder is named after a patron's username and holds that patron's books
- `--settle-seconds <SECONDS>`: Seconds an item must stay unchanged before it is added (5 to 86400; default 60)
- `--scan-interval-seconds <SECONDS>`: Seconds between two full listings of the folder (30 to 86400; default 300)
- `--after-ingest <ACTION>`: What happens to an item once it is added: `move` (to `_imported/`, the default), `delete` or `leave`
- `--ignore <PATTERN>`: Extra file name pattern to ignore, with `*` and `?` (repeatable; replaces the current list)
- `--clear-ignore`: Remove every extra ignore pattern (the built-in list still applies)
- `--prune-imported-days <DAYS>`: With `--after-ingest move`, delete `_imported` day folders older than this many days (1 to 3650)
- `--no-prune`: Keep `_imported` day folders: turn pruning off

##### `bookclerk plugins instances intake clear`

Make a drop folder a destination again. Needs the instance undeployed on every node

```text
bookclerk plugins instances intake clear [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id of the storage instance

### `bookclerk plugins config`

Instance settings, validated against the plugin's declared schema

```text
bookclerk plugins config [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins config get`](#bookclerk-plugins-config-get): Show an instance's effective settings: schema defaults under the operator's document. Secrets show as set or unset, never their value
- [`bookclerk plugins config set`](#bookclerk-plugins-config-set): Set one setting. A `secret` setting takes `--secret` and reads the value from stdin so it never appears on a command line
- [`bookclerk plugins config unset`](#bookclerk-plugins-config-unset): Unset one setting so the schema default applies, or drop a secret

#### `bookclerk plugins config get`

Show an instance's effective settings: schema defaults under the operator's document. Secrets show as set or unset, never their value

```text
bookclerk plugins config get [OPTIONS] <INSTANCE>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id

#### `bookclerk plugins config set`

Set one setting. A `secret` setting takes `--secret` and reads the value from stdin so it never appears on a command line

```text
bookclerk plugins config set [OPTIONS] <INSTANCE> <KEY> [VALUE]
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id
- `<KEY>`: Setting key from the plugin's `[[settings]]`
- `[VALUE]`: Value text: `true`/`false`, a number, or a string. A `table` setting takes a JSON array of row objects, or one row with its cells in column order separated by `|` (an empty cell takes the column default; `\|` is a literal bar)

Options:

- `--secret`: Seal a secret read from stdin
- `--append`: For a `table` setting, add the given rows after the current ones instead of replacing them

#### `bookclerk plugins config unset`

Unset one setting so the schema default applies, or drop a secret

```text
bookclerk plugins config unset [OPTIONS] <INSTANCE> <KEY>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id
- `<KEY>`: Setting key

Options:

- `--row <N>`: For a `table` setting, remove only this row (counted from 1, as `config get` lists them) and keep the others

### `bookclerk plugins approve`

Approve network domains and host bindings for a plugin.

Grants cover the plugin's declared outbound domains and bindings. Operators may add extra network destinations. Structural capabilities cannot be invented beyond the package. Redirect policy is separate.

```text
bookclerk plugins approve [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Plugin id

Options:

- `-y, --yes`: Approve without interactive confirmation (required when stdin is not a TTY)
- `--reset`: Approve the package's request as is, dropping the network destinations you added or denied earlier and your redirect answer. Without it a re-approval keeps them. (An installed version always records its package's request.)

### `bookclerk plugins registry`

Manage configured plugin registries

```text
bookclerk plugins registry [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins registry list`](#bookclerk-plugins-registry-list): List configured registries
- [`bookclerk plugins registry add`](#bookclerk-plugins-registry-add): Add a registry source to `config.toml`
- [`bookclerk plugins registry remove`](#bookclerk-plugins-registry-remove): Remove a registry by index (from `registry list`) or URL

#### `bookclerk plugins registry list`

List configured registries

```text
bookclerk plugins registry list [OPTIONS]
```

#### `bookclerk plugins registry add`

Add a registry source to `config.toml`

```text
bookclerk plugins registry add [OPTIONS] <KIND>
```

Arguments:

- `<KIND>`: Adapter kind (one of `static`, `cargo`, `npm`, `pypi`)

Options:

- `--url <URL>`: Index / registry URL (required for `static`)
- `--name <NAME>`: Optional display name

#### `bookclerk plugins registry remove`

Remove a registry by index (from `registry list`) or URL

```text
bookclerk plugins registry remove [OPTIONS]
```

Options:

- `--index <INDEX>`: Zero-based index from `plugins registry list`
- `--url <URL>`: Match by URL

### `bookclerk plugins db`

Manage isolated plugin database bindings (`capabilities.bindings.databases`)

```text
bookclerk plugins db [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk plugins db list`](#bookclerk-plugins-db-list): List provisioned plugin database bindings
- [`bookclerk plugins db drop`](#bookclerk-plugins-db-drop): Drop provisioned binding units and their registry rows

#### `bookclerk plugins db list`

List provisioned plugin database bindings

```text
bookclerk plugins db list [OPTIONS] [PLUGIN]
```

Arguments:

- `[PLUGIN]`: Limit to one plugin id

#### `bookclerk plugins db drop`

Drop provisioned binding units and their registry rows.

Physically deletes the SQLite file (and journal sidecars), drops the PostgreSQL database, or deletes the Cloudflare D1 database, then removes the registry row. Physical delete must succeed first; unknown adapters fail closed and keep the row.

```text
bookclerk plugins db drop [OPTIONS] <PLUGIN> [BINDING]
```

Arguments:

- `<PLUGIN>`: Plugin id
- `[BINDING]`: Binding name (default: every binding of the plugin)

Options:

- `-y, --yes`: Drop without interactive confirmation

## `bookclerk config`

Read or write configuration values

```text
bookclerk config [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk config get`](#bookclerk-config-get): Print a configuration value by dotted key or classic Settings.json name
- [`bookclerk config set`](#bookclerk-config-set): Set a configuration value and write `config.toml`
- [`bookclerk config show`](#bookclerk-config-show): Print the effective configuration as TOML-ish summary
- [`bookclerk config paths`](#bookclerk-config-paths): Print resolved filesystem paths
- [`bookclerk config master-key`](#bookclerk-config-master-key): Inspect, wrap, move, export or import the library's data-encryption key
- [`bookclerk config template`](#bookclerk-config-template): Naming template helpers
- [`bookclerk config database`](#bookclerk-config-database): Database backend helpers (plugin switch / migration)

### `bookclerk config get`

Print a configuration value by dotted key or classic Settings.json name

```text
bookclerk config get [OPTIONS] [KEY]
```

Arguments:

- `[KEY]`: Dotted key (`output.widevine`) or classic name (`UseWidevine`). Plugin settings are read with `bookclerk plugins config get`

Options:

- `-b, --bare`: Bare list of all classic setting keys and values

### `bookclerk config set`

Set a configuration value and write `config.toml`

```text
bookclerk config set [OPTIONS] <KEY> <VALUE>
```

Arguments:

- `<KEY>`: Dotted key (`library.auto_acquire`) or classic name (`AutoDownloadEpisodes`)
- `<VALUE>`: Value to assign

### `bookclerk config show`

Print the effective configuration as TOML-ish summary

```text
bookclerk config show [OPTIONS]
```

### `bookclerk config paths`

Print resolved filesystem paths

```text
bookclerk config paths [OPTIONS]
```

### `bookclerk config master-key`

Inspect, wrap, move, export or import the library's data-encryption key

```text
bookclerk config master-key [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk config master-key status`](#bookclerk-config-master-key-status): Show where the key is kept (the file, the OS keychain or systemd-creds) and whether it is raw (BCK1) or wrapped (BCK2)
- [`bookclerk config master-key wrap`](#bookclerk-config-master-key-wrap): Wrap a BCK1 key with a passphrase (no-op unlock if already BCK2), in `master.key` or in its OS keychain item
- [`bookclerk config master-key move`](#bookclerk-config-master-key-move): Move the key to the OS keychain, into a systemd-creds credential, or back into `master.key`, and record the choice as `[auth].key_store` in config.toml
- [`bookclerk config master-key export`](#bookclerk-config-master-key-export): Write a recovery copy of the key, wrapped with a password read from stdin (never argv)
- [`bookclerk config master-key credential`](#bookclerk-config-master-key-credential): Write the key's envelope, exactly as stored (BCK1, or BCK2 under the auth password), to stdout for `systemd-creds encrypt`
- [`bookclerk config master-key import`](#bookclerk-config-master-key-import): Restore the key from a recovery copy (or a copied `master.key`) on an install that has no `master.key`. A wrapped copy's password is read from stdin

#### `bookclerk config master-key status`

Show where the key is kept (the file, the OS keychain or systemd-creds) and whether it is raw (BCK1) or wrapped (BCK2)

```text
bookclerk config master-key status [OPTIONS]
```

#### `bookclerk config master-key wrap`

Wrap a BCK1 key with a passphrase (no-op unlock if already BCK2), in `master.key` or in its OS keychain item.

Password from `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` — never argv.

```text
bookclerk config master-key wrap [OPTIONS]
```

#### `bookclerk config master-key move`

Move the key to the OS keychain, into a systemd-creds credential, or back into `master.key`, and record the choice as `[auth].key_store` in config.toml.

The key itself doesn't change, so every sealed secret stays readable. Stop `bookclerkd` first.

```text
bookclerk config master-key move [OPTIONS] <TO>
```

Arguments:

- `<TO>`: Where to keep the key: `keychain`, `systemd-creds` or `file`

#### `bookclerk config master-key export`

Write a recovery copy of the key, wrapped with a password read from stdin (never argv).

Keep it apart from the server's files: with it and its password, the library's sealed secrets can be read.

```text
bookclerk config master-key export [OPTIONS] --out <OUT>
```

Options:

- `--out <OUT>`: File to write (created with mode 0600; never overwritten) (required)

#### `bookclerk config master-key credential`

Write the key's envelope, exactly as stored (BCK1, or BCK2 under the auth password), to stdout for `systemd-creds encrypt`.

For administrators who seal the key into a unit credential themselves (systemd older than 256, or a system credential store): pipe it into `systemd-creds encrypt --name=bookclerk-master-key - <file>`, load that file with `LoadCredentialEncrypted=` in the unit, and set `[auth].key_store = "systemd-creds"`. The next start adopts the credential. Refuses to write to a terminal.

```text
bookclerk config master-key credential [OPTIONS]
```

#### `bookclerk config master-key import`

Restore the key from a recovery copy (or a copied `master.key`) on an install that has no `master.key`. A wrapped copy's password is read from stdin

```text
bookclerk config master-key import [OPTIONS] --from <FROM>
```

Options:

- `--from <FROM>`: The recovery copy to read (required)

### `bookclerk config template`

Naming template helpers

```text
bookclerk config template [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk config template tags`](#bookclerk-config-template-tags): List supported naming template property tags
- [`bookclerk config template profiles`](#bookclerk-config-template-profiles): List built-in naming profiles and their templates
- [`bookclerk config template preview`](#bookclerk-config-template-preview): Preview folder/file templates for a library title

#### `bookclerk config template tags`

List supported naming template property tags

```text
bookclerk config template tags [OPTIONS]
```

#### `bookclerk config template profiles`

List built-in naming profiles and their templates

```text
bookclerk config template profiles [OPTIONS]
```

#### `bookclerk config template preview`

Preview folder/file templates for a library title

```text
bookclerk config template preview [OPTIONS] <ASIN>
```

Arguments:

- `<ASIN>`: Title ASIN from the local library DB

Options:

- `--account <ACCOUNT>`: Account id when the ASIN exists on multiple accounts
- `--profile <PROFILE>`: Override `output.naming_profile` for this preview
- `--folder <FOLDER>`: Override `output.folder_template` for this preview
- `--file <FILE>`: Override `output.file_template` for this preview
- `--ext <EXT>`: File extension (default: m4b)

### `bookclerk config database`

Database backend helpers (plugin switch / migration)

```text
bookclerk config database [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk config database migrate`](#bookclerk-config-database-migrate): Copy library data from one `[database].plugin` backend to another
- [`bookclerk config database setup`](#bookclerk-config-database-setup): Guided setup for a hosted database: reads a Cloudflare API token from stdin (never argv), finds or creates the D1 database, mints a token limited to D1, seals it with the library's key in the master key's store (the file, the OS keychain or systemd-creds), and writes `[database.d1]` to config.toml

#### `bookclerk config database migrate`

Copy library data from one `[database].plugin` backend to another.

Does not change `config.toml` unless `--apply` is passed after a successful copy.

```text
bookclerk config database migrate [OPTIONS] --to <TO>
```

Options:

- `--from <FROM>`: Source plugin id (`sqlite`, `d1`, `postgres`). Default: current `[database].plugin`
- `--to <TO>`: Destination plugin id (required)
- `--dry-run`: Report row counts only; do not write to the destination
- `--force`: Allow copying into a destination that already has library rows
- `--apply`: After a successful migrate, set `[database].plugin` to `--to` and write config.toml

#### `bookclerk config database setup`

Guided setup for a hosted database: reads a Cloudflare API token from stdin (never argv), finds or creates the D1 database, mints a token limited to D1, seals it with the library's key in the master key's store (the file, the OS keychain or systemd-creds), and writes `[database.d1]` to config.toml.

The pasted token is not stored. `[database].plugin` is not changed: move the library with `config database migrate --to d1 --apply`.

```text
bookclerk config database setup [OPTIONS] <BACKEND>
```

Arguments:

- `<BACKEND>`: The hosted database to set up (one of `d1`)

Options:

- `--account-id <ACCOUNT_ID>`: Cloudflare account id; found from the token when it sees only one
- `--database-name <DATABASE_NAME>`: D1 database name (default `bookclerk`); created when missing
- `--jurisdiction <JURISDICTION>`: Where a new database keeps its data: `default`, `eu` or `fedramp`

## `bookclerk export`

Export library data, backups, or Libation-compatible files

```text
bookclerk export [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk export native`](#bookclerk-export-native): Write a portable Bookclerk `.tar.gz` backup of the files directory
- [`bookclerk export libation`](#bookclerk-export-libation): Export Settings.json, AccountsSettings.json, and LibationContext.db
- [`bookclerk export library`](#bookclerk-export-library): Export library titles as CSV / JSON / XLSX: the whole library, or with `--user` that person's view with their tags and finished flags
- [`bookclerk export postgres`](#bookclerk-export-postgres): Copy library.db to PostgreSQL

### `bookclerk export native`

Write a portable Bookclerk `.tar.gz` backup of the files directory

```text
bookclerk export native [OPTIONS] --path <PATH>
```

Options:

- `-p, --path <PATH>`: Destination archive path (required)
- `--include-plugin-manifests`: Include `plugins/**/plugin.toml` (not binaries)
- `--include-cache`: Include `cache/` (large)
- `--include-logs`: Include `logs/`
- `--include-plugin-databases`: Include `plugin-databases/` (SQLite binding files)

### `bookclerk export libation`

Export Settings.json, AccountsSettings.json, and LibationContext.db

```text
bookclerk export libation [OPTIONS] --path <PATH>
```

Options:

- `-p, --path <PATH>`: Destination classic Libation Files directory (required)
- `--force`: Overwrite an existing destination directory
- `--dry-run`: Report only; do not write
- `--user <USER>`: Write this person's tags, ratings and finished flags (login name or user id); without it they are empty

### `bookclerk export library`

Export library titles as CSV / JSON / XLSX: the whole library, or with `--user` that person's view with their tags and finished flags

```text
bookclerk export library [OPTIONS] --path <PATH> [ASINS]...
```

Arguments:

- `[ASINS]...`: Limit to specific ASINs / title ids

Options:

- `-p, --path <PATH>`: Output file path (required)
- `--csv`: Force CSV even when the destination extension is not `.csv`
- `--json`: Force JSON even when the destination extension is not `.json`
- `--xlsx`: Force XLSX even when the destination extension is not `.xlsx`
- `--account <ACCOUNT>`: Limit exported library rows to this account id when set
- `--user <USER>`: Fill `tags` and `is_finished` with this person's own state (login name or user id); without it they are empty

### `bookclerk export postgres`

Copy library.db to PostgreSQL

```text
bookclerk export postgres [OPTIONS] --connection <CONNECTION>
```

Options:

- `-c, --connection <CONNECTION>`: PostgreSQL connection string (required)
- `--source <SOURCE>`: Source SQLite path (default: `{files_dir}/library.db`)
- `--format <FORMAT>`: Output schema format (one of `classic`, `flat`; default `classic`)

## `bookclerk import`

Import native backups or classic Libation Files

```text
bookclerk import [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk import native`](#bookclerk-import-native): Restore a native Bookclerk `.tar.gz` backup into the files directory
- [`bookclerk import libation`](#bookclerk-import-libation): Import classic Libation Files (Settings / accounts / DB)

### `bookclerk import native`

Restore a native Bookclerk `.tar.gz` backup into the files directory

```text
bookclerk import native [OPTIONS] --from <ARCHIVE>
```

Options:

- `--from <ARCHIVE>`: Archive path (required)
- `--force`: Overwrite existing files
- `--dry-run`: Report only; do not write

### `bookclerk import libation`

Import classic Libation Files (Settings / accounts / DB)

```text
bookclerk import libation [OPTIONS] --from <FROM>
```

Options:

- `--from <FROM>`: Classic Libation Files directory (or `BOOKCLERK_CLASSIC_FILES`) (required; environment variable `BOOKCLERK_CLASSIC_FILES`)
- `--force`: Overwrite existing `config.toml`
- `--skip-auth`: No-op retained for compatibility (credentials are never imported by migrate)
- `--dry-run`: Print what would be imported without writing

## `bookclerk db`

Host schema version, backups, and explicit migrate

```text
bookclerk db [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk db version`](#bookclerk-db-version): Show this binary's frozen plan and the database's explicit schema state
- [`bookclerk db backup`](#bookclerk-db-backup): Canonical Bookclerk backup (logically full recovery points)
- [`bookclerk db restore`](#bookclerk-db-restore): Restore schema and data from a recovery-point id, archive, or timestamp
- [`bookclerk db migrate`](#bookclerk-db-migrate): Apply frozen ups or reversible downs to reach `--to`

### `bookclerk db version`

Show this binary's frozen plan and the database's explicit schema state

```text
bookclerk db version [OPTIONS]
```

### `bookclerk db backup`

Canonical Bookclerk backup (logically full recovery points)

```text
bookclerk db backup [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk db backup create`](#bookclerk-db-backup-create): Write a manual recovery point under `files_dir/backups/`
- [`bookclerk db backup list`](#bookclerk-db-backup-list): List recovery points oldest-first
- [`bookclerk db backup verify`](#bookclerk-db-backup-verify): Verify a recovery point (objects, schema, typed rows) without restoring
- [`bookclerk db backup prune`](#bookclerk-db-backup-prune): Prune automatic pre-migrate recovery points and unreferenced objects

#### `bookclerk db backup create`

Write a manual recovery point under `files_dir/backups/`

```text
bookclerk db backup create [OPTIONS]
```

Options:

- `--path <PATH>`: Optional `.tar.gz` destination for this recovery point
- `--include-plugin-databases`: Capture plugin-owned database bindings in portable Bookclerk SQL. Plugin schema migration remains plugin-owned. Each database unit is replaced completely; a multi-database bundle is not one transaction.

#### `bookclerk db backup list`

List recovery points oldest-first

```text
bookclerk db backup list [OPTIONS]
```

#### `bookclerk db backup verify`

Verify a recovery point (objects, schema, typed rows) without restoring

```text
bookclerk db backup verify [OPTIONS] --from <FROM>
```

Options:

- `--from <FROM>`: Recovery-point id, archive, or `created_at` prefix (required)

#### `bookclerk db backup prune`

Prune automatic pre-migrate recovery points and unreferenced objects.

Manual backups are never removed.

```text
bookclerk db backup prune [OPTIONS]
```

### `bookclerk db restore`

Restore schema and data from a recovery-point id, archive, or timestamp

```text
bookclerk db restore [OPTIONS] --from <FROM>
```

Options:

- `--from <FROM>`: Recovery-point id, `.tar.gz` archive, or `created_at` prefix. Restores onto the currently configured database adapter (not necessarily the adapter that captured the backup). (required)

### `bookclerk db migrate`

Apply frozen ups or reversible downs to reach `--to`

```text
bookclerk db migrate [OPTIONS]
```

Options:

- `--to <TO>`: Target frozen schema version (defaults to this binary's `SCHEMA_VERSION`)
- `--include-plugin-databases`: Include plugin-owned database bindings in the pre-migrate backup

## `bookclerk daemon`

Talk to a running bookclerkd control plane

```text
bookclerk daemon [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk daemon health`](#bookclerk-daemon-health): GET /health
- [`bookclerk daemon status`](#bookclerk-daemon-status): GET /status (or /api/status)
- [`bookclerk daemon scan`](#bookclerk-daemon-scan): POST /scan (or /api/library/scan)
- [`bookclerk daemon acquire`](#bookclerk-daemon-acquire): POST /acquire (or /api/library/acquire)
- [`bookclerk daemon jobs`](#bookclerk-daemon-jobs): GET /jobs (or /api/jobs)
- [`bookclerk daemon token`](#bookclerk-daemon-token): Show or rotate the operator API token (DB-backed; env override wins)

### `bookclerk daemon health`

GET /health

```text
bookclerk daemon health [OPTIONS]
```

### `bookclerk daemon status`

GET /status (or /api/status)

```text
bookclerk daemon status [OPTIONS]
```

### `bookclerk daemon scan`

POST /scan (or /api/library/scan)

```text
bookclerk daemon scan [OPTIONS]
```

Options:

- `--account <ACCOUNT>`: Optional account id forwarded in the scan JSON body

### `bookclerk daemon acquire`

POST /acquire (or /api/library/acquire)

```text
bookclerk daemon acquire [OPTIONS]
```

Options:

- `--asin <ASIN>`: Optional product id (ASIN/ISBN) forwarded in the acquire JSON body
- `--account <ACCOUNT>`: Optional account id forwarded in the acquire JSON body

### `bookclerk daemon jobs`

GET /jobs (or /api/jobs)

```text
bookclerk daemon jobs [OPTIONS]
```

### `bookclerk daemon token`

Show or rotate the operator API token (DB-backed; env override wins)

```text
bookclerk daemon token [OPTIONS] [COMMAND]
```

Commands:

- [`bookclerk daemon token show`](#bookclerk-daemon-token-show): Print the current effective operator token (default)
- [`bookclerk daemon token rotate`](#bookclerk-daemon-token-rotate): Mint a new token, store it in encrypted_secrets, and reload the daemon when reachable

#### `bookclerk daemon token show`

Print the current effective operator token (default)

```text
bookclerk daemon token show [OPTIONS]
```

#### `bookclerk daemon token rotate`

Mint a new token, store it in encrypted_secrets, and reload the daemon when reachable

```text
bookclerk daemon token rotate [OPTIONS]
```

## `bookclerk events`

Durable domain-event outbox and deliveries

```text
bookclerk events [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk events list`](#bookclerk-events-list): GET /api/events — recent outbox envelopes
- [`bookclerk events dead-letters`](#bookclerk-events-dead-letters): GET /api/events/deliveries?state=dead_letter
- [`bookclerk events retry`](#bookclerk-events-retry): POST /api/events/deliveries/{id}/retry
- [`bookclerk events ack`](#bookclerk-events-ack): POST /api/events/deliveries/{id}/acknowledge
- [`bookclerk events cancel`](#bookclerk-events-cancel): POST /api/events/deliveries/{id}/cancel
- [`bookclerk events resume`](#bookclerk-events-resume): POST /api/events/deliveries/{id}/resume

### `bookclerk events list`

GET /api/events — recent outbox envelopes

```text
bookclerk events list [OPTIONS]
```

### `bookclerk events dead-letters`

GET /api/events/deliveries?state=dead_letter

```text
bookclerk events dead-letters [OPTIONS]
```

### `bookclerk events retry`

POST /api/events/deliveries/{id}/retry

```text
bookclerk events retry [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Delivery id (`{event_id}:{plugin_id}`)

### `bookclerk events ack`

POST /api/events/deliveries/{id}/acknowledge

```text
bookclerk events ack [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Delivery id (`{event_id}:{plugin_id}`)

### `bookclerk events cancel`

POST /api/events/deliveries/{id}/cancel

```text
bookclerk events cancel [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Delivery id (`{event_id}:{plugin_id}`)

### `bookclerk events resume`

POST /api/events/deliveries/{id}/resume

```text
bookclerk events resume [OPTIONS] <ID>
```

Arguments:

- `<ID>`: Delivery id (`{event_id}:{plugin_id}`)

## `bookclerk libraries`

Owner libraries this install hosts for, and consent to their plugins

```text
bookclerk libraries [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk libraries list`](#bookclerk-libraries-list): The libraries this install is enrolled in as a host
- [`bookclerk libraries join`](#bookclerk-libraries-join): Enroll this install as a host (a branch) of an owner library
- [`bookclerk libraries leave`](#bookclerk-libraries-leave): Leave a library: the owner is told (and revokes this node when it hears), the link stops being dialed and its node keys are deleted
- [`bookclerk libraries consents`](#bookclerk-libraries-consents): What each library asks to run here, and whether it may
- [`bookclerk libraries approve`](#bookclerk-libraries-approve): Approve an owner plugin's request to run here, optionally narrowed. Name the request you reviewed by the digest `libraries consents` shows (it covers the version, the grant, the archive URL, the archive digest and the payload digest); it is refused when the library has changed the request since. Only what the request asks for can be approved; a later version that asks for more needs approval again
- [`bookclerk libraries scan`](#bookclerk-libraries-scan): Ask a library to scan its library for this node's owner
- [`bookclerk libraries revoke`](#bookclerk-libraries-revoke): Withdraw an approval: held jobs of the plugin are cancelled and given back, and nothing of it is claimed until this host's operator approves it again (a platform plugin is not approved automatically)

### `bookclerk libraries list`

The libraries this install is enrolled in as a host

```text
bookclerk libraries list [OPTIONS]
```

### `bookclerk libraries join`

Enroll this install as a host (a branch) of an owner library.

With `--url`, prints a sign-in link (and opens a browser when it can): a user of that library signs in and approves, and the answer comes back to a loopback listener this command opens. Without it, reads a branch invite (`https://<owner>/enroll#bcn1...`) from stdin, or prompts for it; an invite is never taken from the command line.

```text
bookclerk libraries join [OPTIONS]
```

Options:

- `--url <URL>`: The owner library's address (`https://library.example`)
- `--name <NAME>`: The name the owner sees for this branch (default: host name)
- `--fingerprint <FINGERPRINT>`: The library's certificate fingerprint as Settings → Branches shows it (a `sha256:` prefix is accepted), confirming a self-signed library without a prompt
- `--as <JOIN_AS>`: How to join: `branch` (default; keeps this install's own database) or `shared_branch` (shares the library's database and key; only an empty install, and only when an Owner of the library chooses it after a fresh second factor) (one of `branch`, `shared-branch`)

### `bookclerk libraries leave`

Leave a library: the owner is told (and revokes this node when it hears), the link stops being dialed and its node keys are deleted

```text
bookclerk libraries leave [OPTIONS]
```

Options:

- `--library <LIBRARY>`: Library id (default: the only enrolled library)

### `bookclerk libraries consents`

What each library asks to run here, and whether it may

```text
bookclerk libraries consents [OPTIONS]
```

Options:

- `--library <LIBRARY>`: Only this library

### `bookclerk libraries approve`

Approve an owner plugin's request to run here, optionally narrowed. Name the request you reviewed by the digest `libraries consents` shows (it covers the version, the grant, the archive URL, the archive digest and the payload digest); it is refused when the library has changed the request since. Only what the request asks for can be approved; a later version that asks for more needs approval again

```text
bookclerk libraries approve [OPTIONS] --request <DIGEST> <PLUGIN>
```

Arguments:

- `<PLUGIN>`: Canonical plugin key

Options:

- `--library <LIBRARY>`: Library id (default: the only enrolled library)
- `--request <DIGEST>`: The `request_digest` of the request you reviewed, from `libraries consents` (refused when the library now asks for anything else) (required)
- `--deny-domain <DOMAIN>`: Take a domain out of the request. Repeatable
- `--deny-binding <BINDING>`: Take a binding out of the request (`secrets`, `kv`, ...). Repeatable
- `--deny-own-user-agent <HOST>`: Refuse the plugin's own User-Agent on a host it asks for; its requests there carry Bookclerk's agent. Repeatable
- `--deny-network`: Deny all network access

### `bookclerk libraries scan`

Ask a library to scan its library for this node's owner.

Goes through this install's running daemon, over its link to the owner. The owner plans it as that user: an Owner or Administrator scanning every account gets one job per account, spread over the library's nodes and hosts; anyone else scans only their own accounts. Waits for the jobs unless `--no-wait`.

```text
bookclerk libraries scan [OPTIONS]
```

Options:

- `--library <LIBRARY>`: Library id (default: the only enrolled library)
- `--account <ACCOUNT>`: Scan only this store account of the library
- `--instance <INSTANCE>`: Pin the scan to one owner plugin instance
- `--no-wait`: Print the queued job ids without waiting for them

### `bookclerk libraries revoke`

Withdraw an approval: held jobs of the plugin are cancelled and given back, and nothing of it is claimed until this host's operator approves it again (a platform plugin is not approved automatically)

```text
bookclerk libraries revoke [OPTIONS] <PLUGIN>
```

Arguments:

- `<PLUGIN>`: Canonical plugin key

Options:

- `--library <LIBRARY>`: Library id (default: the only enrolled library)

## `bookclerk ingress`

The library's public address from an ingress plugin

```text
bookclerk ingress [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk ingress status`](#bookclerk-ingress-status): Show the active ingress, what it exposes and the public origin
- [`bookclerk ingress activate`](#bookclerk-ingress-activate): Make a plugin instance the library's active ingress. Needs an Owner account and operator auth on
- [`bookclerk ingress deactivate`](#bookclerk-ingress-deactivate): Turn ingress off. The adopted origin is kept for next time
- [`bookclerk ingress exposure`](#bookclerk-ingress-exposure): Choose what public traffic may reach
- [`bookclerk ingress accept-origin`](#bookclerk-ingress-accept-origin): Adopt the new origin the active ingress reported. Passkeys and sign-in redirects registered under the old name stop working

### `bookclerk ingress status`

Show the active ingress, what it exposes and the public origin

```text
bookclerk ingress status [OPTIONS]
```

### `bookclerk ingress activate`

Make a plugin instance the library's active ingress. Needs an Owner account and operator auth on

```text
bookclerk ingress activate [OPTIONS] <PLUGIN>
```

Arguments:

- `<PLUGIN>`: A plugin instance UUID, or a plugin key for its default instance

### `bookclerk ingress deactivate`

Turn ingress off. The adopted origin is kept for next time

```text
bookclerk ingress deactivate [OPTIONS]
```

### `bookclerk ingress exposure`

Choose what public traffic may reach

```text
bookclerk ingress exposure [OPTIONS] <EXPOSURE>
```

Arguments:

- `<EXPOSURE>`: `branches` (the default) or `library` (one of `branches`, `library`)

### `bookclerk ingress accept-origin`

Adopt the new origin the active ingress reported. Passkeys and sign-in redirects registered under the old name stop working

```text
bookclerk ingress accept-origin [OPTIONS]
```

## `bookclerk nodes`

Library nodes: liveness, capacity, budgets, and placement labels

```text
bookclerk nodes [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk nodes list`](#bookclerk-nodes-list): Every registered node: liveness, artifact target, and labels
- [`bookclerk nodes set`](#bookclerk-nodes-set): Set a node's scheduling budget or drain it. Applies at that node's next heartbeat
- [`bookclerk nodes label`](#bookclerk-nodes-label): Add or remove placement labels on a node (`nas`, `zone=eu`)
- [`bookclerk nodes owner`](#bookclerk-nodes-owner): Assign a node to the user who administers it, or clear its owner. The owner (any active user) may then change the node's budgets, drain switch, and name. Work the node is trusted with still follows the owner's role
- [`bookclerk nodes invite`](#bookclerk-nodes-invite): Mint a single-use link that enrolls one branch node for a user. The link (with its secret in the URL fragment) is printed once
- [`bookclerk nodes invites`](#bookclerk-nodes-invites): List or revoke enrollment links
- [`bookclerk nodes revoke`](#bookclerk-nodes-revoke): Revoke an enrolled branch node's credential. Its connection closes and it receives no more work

### `bookclerk nodes list`

Every registered node: liveness, artifact target, and labels

```text
bookclerk nodes list [OPTIONS]
```

### `bookclerk nodes set`

Set a node's scheduling budget or drain it. Applies at that node's next heartbeat

```text
bookclerk nodes set [OPTIONS] [NODE]
```

Arguments:

- `[NODE]`: Node id (default: this node)

Options:

- `--cpu-budget <PERCENT>`: Share of detected CPU the scheduler may commit here (1-100)
- `--memory-budget <PERCENT>`: Share of detected memory the scheduler may commit here (1-100)
- `--drain`: Place nothing new here and move scheduled replicas away
- `--undrain`: Accept scheduled replicas again
- `--label <LABEL>`: Operator label for the node
- `--cache-budget <BYTES>`: Disk space, in bytes, this node's image caches may use, overriding the library's `cache.budget_bytes` (16 MiB to 1 TiB)
- `--library-cache-budget`: Drop this node's cache budget override and follow the library's

### `bookclerk nodes label`

Add or remove placement labels on a node (`nas`, `zone=eu`)

```text
bookclerk nodes label [OPTIONS] [NODE]
```

Arguments:

- `[NODE]`: Node id (default: this node)

Options:

- `--add <LABEL>`: Label to add. Repeatable
- `--remove <LABEL>`: Label to remove. Repeatable
- `--clear`: Start from no labels (then apply `--add`)

### `bookclerk nodes owner`

Assign a node to the user who administers it, or clear its owner. The owner (any active user) may then change the node's budgets, drain switch, and name. Work the node is trusted with still follows the owner's role

```text
bookclerk nodes owner [OPTIONS] [NODE]
```

Arguments:

- `[NODE]`: Node id (default: this node)

Options:

- `--user <USER>`: Login name or user id
- `--clear`: Remove the node's owner

### `bookclerk nodes invite`

Mint a single-use link that enrolls one branch node for a user. The link (with its secret in the URL fragment) is printed once

```text
bookclerk nodes invite [OPTIONS] --user <USER>
```

Options:

- `--user <USER>`: Login name or user id of the user who will own the branch node (required)
- `--ttl <TTL>`: How long the link stays valid (`30m`, `24h`, `7d`; 5m to 7d) (default `24h`)
- `--label <LABEL>`: Label shown in `nodes invites list` (default ``)

### `bookclerk nodes invites`

List or revoke enrollment links

```text
bookclerk nodes invites [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk nodes invites list`](#bookclerk-nodes-invites-list): Every enrollment link and its state. Secrets are never shown again
- [`bookclerk nodes invites revoke`](#bookclerk-nodes-invites-revoke): Revoke a link that was not redeemed yet

#### `bookclerk nodes invites list`

Every enrollment link and its state. Secrets are never shown again

```text
bookclerk nodes invites list [OPTIONS]
```

#### `bookclerk nodes invites revoke`

Revoke a link that was not redeemed yet

```text
bookclerk nodes invites revoke [OPTIONS] <LINK_ID>
```

Arguments:

- `<LINK_ID>`: Link id

### `bookclerk nodes revoke`

Revoke an enrolled branch node's credential. Its connection closes and it receives no more work

```text
bookclerk nodes revoke [OPTIONS] <NODE>
```

Arguments:

- `<NODE>`: Branch node id

## `bookclerk users`

The library's people: patron scopes, member controls and Administrator switches

```text
bookclerk users [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk users scope`](#bookclerk-users-scope): Read or set which part of the library a patron sees
- [`bookclerk users controls`](#bookclerk-users-controls): Read or set what a patron may do: link store accounts, download, edit metadata, upload, request books, see prices
- [`bookclerk users admin-controls`](#bookclerk-users-admin-controls): Read or set what an Administrator may do: manage members, set patron limits, library-wide scan and acquire, run the job queue, fix the catalog, invite branches, approve requests

### `bookclerk users scope`

Read or set which part of the library a patron sees

```text
bookclerk users scope [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk users scope get`](#bookclerk-users-scope-get): Print a patron's scope, or the library's default
- [`bookclerk users scope set`](#bookclerk-users-scope-set): Replace a patron's scope, or the library's default. With no `--include` or `--exclude`, the scope hides nothing

#### `bookclerk users scope get`

Print a patron's scope, or the library's default

```text
bookclerk users scope get [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: The library's default scope, which patrons without their own follow

#### `bookclerk users scope set`

Replace a patron's scope, or the library's default. With no `--include` or `--exclude`, the scope hides nothing

```text
bookclerk users scope set [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: Set the library's default scope instead of one patron's
- `--include <FIELD=VALUE>`: Show only books with this value (`genre=Kids`); fields are genre, series, author, narrator, tag (a library tag) and collection. Repeatable: a book needs one
- `--exclude <FIELD=VALUE>`: Hide books with this value, whatever `--include` says. Repeatable
- `--hide-explicit`: Hide books a store or metadata file marks as explicit content
- `--follow-default`: Drop the patron's own scope so they follow the library's default

### `bookclerk users controls`

Read or set what a patron may do: link store accounts, download, edit metadata, upload, request books, see prices

```text
bookclerk users controls [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk users controls get`](#bookclerk-users-controls-get): Print a patron's controls, or the library's default
- [`bookclerk users controls set`](#bookclerk-users-controls-set): Change controls of a patron, or of the library's default. Controls not named keep their value

#### `bookclerk users controls get`

Print a patron's controls, or the library's default

```text
bookclerk users controls get [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: The library's default, which patrons follow unless a control is set for them alone

#### `bookclerk users controls set`

Change controls of a patron, or of the library's default. Controls not named keep their value

```text
bookclerk users controls set [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: Change the library's default instead of one patron's controls
- `--allow <CONTROL>`: Allow a control (`link_accounts`, `download`, `edit_metadata`, `upload`, `request_books`, `see_prices`). Repeatable
- `--deny <CONTROL>`: Turn a control off. Repeatable
- `--follow-default <CONTROL>`: Make the patron follow the library's default for a control again. Repeatable; `all` names every control

### `bookclerk users admin-controls`

Read or set what an Administrator may do: manage members, set patron limits, library-wide scan and acquire, run the job queue, fix the catalog, invite branches, approve requests

```text
bookclerk users admin-controls [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk users admin-controls get`](#bookclerk-users-admin-controls-get): Print an Administrator's switches, or the library's default
- [`bookclerk users admin-controls set`](#bookclerk-users-admin-controls-set): Change switches of an Administrator, or of the library's default. Switches not named keep their value

#### `bookclerk users admin-controls get`

Print an Administrator's switches, or the library's default

```text
bookclerk users admin-controls get [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: The library's default, which Administrators follow unless a switch is set for them alone

#### `bookclerk users admin-controls set`

Change switches of an Administrator, or of the library's default. Switches not named keep their value

```text
bookclerk users admin-controls set [OPTIONS] [USER]
```

Arguments:

- `[USER]`: Login name or user id

Options:

- `--default`: Change the library's default instead of one Administrator's
- `--allow <SWITCH>`: Turn a switch on (`manage_members`, `set_patron_limits`, `library_scan`, `run_jobs`, `fix_catalog`, `invite_branches`, `approve_requests`). Repeatable
- `--deny <SWITCH>`: Turn a switch off. Repeatable
- `--follow-default <SWITCH>`: Make the Administrator follow the library's default for a switch again. Repeatable; `all` names every switch

## `bookclerk queues`

Plugin-owned queues on the outbox: counters, consumers, messages

```text
bookclerk queues [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk queues list`](#bookclerk-queues-list): GET /api/queues — every queue with delivery counters and consumers
- [`bookclerk queues inspect`](#bookclerk-queues-inspect): GET /api/queues/{name} — counters, consumers, recent messages and deliveries

### `bookclerk queues list`

GET /api/queues — every queue with delivery counters and consumers

```text
bookclerk queues list [OPTIONS]
```

### `bookclerk queues inspect`

GET /api/queues/{name} — counters, consumers, recent messages and deliveries

```text
bookclerk queues inspect [OPTIONS] <NAME>
```

Arguments:

- `<NAME>`: Queue name (`core.events` is the domain-event bus)

## `bookclerk media`

What this build's media worker reads and writes

```text
bookclerk media [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk media capabilities`](#bookclerk-media-capabilities): List the audio codecs, file types and output formats this build's media worker handles (the daemon's `GET /api/status/media`)

### `bookclerk media capabilities`

List the audio codecs, file types and output formats this build's media worker handles (the daemon's `GET /api/status/media`)

```text
bookclerk media capabilities [OPTIONS]
```

## `bookclerk login`

Print a loopback operator sign-in URL (requires a running daemon)

```text
bookclerk login [OPTIONS]
```

## `bookclerk diagnostics`

Diagnostics ring buffer and opt-in upload

```text
bookclerk diagnostics [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk diagnostics show`](#bookclerk-diagnostics-show): Print recent diagnostics ring-buffer events
- [`bookclerk diagnostics upload`](#bookclerk-diagnostics-upload): Upload the current diagnostics ring (requires share_reports + collector URL)
- [`bookclerk diagnostics reset-id`](#bookclerk-diagnostics-reset-id): Replace this install's random usage ID (diagnostics.share_usage) with a new one
- [`bookclerk diagnostics reports`](#bookclerk-diagnostics-reports): List the diagnostics reports this install sent, newest first
- [`bookclerk diagnostics view`](#bookclerk-diagnostics-view): Read a sent report back from the collector, with the issues tracking it and its fix
- [`bookclerk diagnostics delete`](#bookclerk-diagnostics-delete): Delete the collector's stored copy of a sent report

### `bookclerk diagnostics show`

Print recent diagnostics ring-buffer events

```text
bookclerk diagnostics show [OPTIONS]
```

Options:

- `-n, --limit <LIMIT>`: Max events to print (0 = all) (default `50`)

### `bookclerk diagnostics upload`

Upload the current diagnostics ring (requires share_reports + collector URL)

```text
bookclerk diagnostics upload [OPTIONS]
```

Options:

- `--trigger <TRIGGER>`: Trigger label recorded with the report (default `cli_manual`)

### `bookclerk diagnostics reset-id`

Replace this install's random usage ID (diagnostics.share_usage) with a new one.

The next check-in then counts as a new install. Restart the daemon so its reports carry the new ID.

```text
bookclerk diagnostics reset-id [OPTIONS]
```

### `bookclerk diagnostics reports`

List the diagnostics reports this install sent, newest first.

Each report ID is what the collector stored and what the GitHub issue for it mentions: search the issues for it, or give it to a maintainer.

The access code beside each ID reads the report back (`diagnostics view`), deletes the collector's copy (`diagnostics delete`), and with the ID opens the collector's public lookup page.

```text
bookclerk diagnostics reports [OPTIONS]
```

Options:

- `-n, --limit <LIMIT>`: Max reports to print (0 = all that are kept) (default `20`)

### `bookclerk diagnostics view`

Read a sent report back from the collector, with the issues tracking it and its fix.

Uses the access code kept in this install's report history.

```text
bookclerk diagnostics view [OPTIONS] <REPORT_ID>
```

Arguments:

- `<REPORT_ID>`: The report's ID, from `bookclerk diagnostics reports`

### `bookclerk diagnostics delete`

Delete the collector's stored copy of a sent report.

The collector keeps the report's one-line summary and issue links, so it still counts toward the issue it joined.

```text
bookclerk diagnostics delete [OPTIONS] <REPORT_ID>
```

Arguments:

- `<REPORT_ID>`: The report's ID, from `bookclerk diagnostics reports`

Options:

- `-y, --yes`: Delete without asking (required when stdin is not a terminal)

## `bookclerk version`

Print version information

```text
bookclerk version [OPTIONS]
```
