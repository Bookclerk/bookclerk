# CLI reference

Every command and option of the `bookclerk` command-line tool. During
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

These options work with every command.

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
- [`bookclerk clusters`](#bookclerk-clusters): Owner clusters this install hosts for, and consent to their plugins
- [`bookclerk nodes`](#bookclerk-nodes): Cluster nodes: liveness, capacity, budgets, and placement labels
- [`bookclerk queues`](#bookclerk-queues): Plugin-owned queues on the outbox: counters, consumers, messages
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
- [`bookclerk library status`](#bookclerk-library-status): Show token validity / refresh health across sources
- [`bookclerk library revoke-account`](#bookclerk-library-revoke-account): Remove store credentials but keep acquired books and account rows
- [`bookclerk library scan`](#bookclerk-library-scan): Sync the library from the content sources: queues one scan job per account (every scan-enabled account when none is named) for a bookclerkd node to run, and waits for them
- [`bookclerk library acquire`](#bookclerk-library-acquire): Download + decrypt + store titles: queues one acquire job per title id (or one for every pending title) for a bookclerkd node to run, and waits for them. `--dry-run` only prints planned keys, here
- [`bookclerk library set-status`](#bookclerk-library-set-status): Match storage files to library rows and update acquire status
- [`bookclerk library get-license`](#bookclerk-library-get-license): Fetch a content license for an ASIN
- [`bookclerk library search`](#bookclerk-library-search): Search the library index
- [`bookclerk library export`](#bookclerk-library-export): Export library rows as CSV / JSON / XLSX (alias of `export library`)
- [`bookclerk library list`](#bookclerk-library-list): List books in the local library DB
- [`bookclerk library convert`](#bookclerk-library-convert): Convert acquired m4b/m4a to mp3
- [`bookclerk library filters`](#bookclerk-library-filters): Manage saved quick filters

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

Download + decrypt + store titles: queues one acquire job per title id (or one for every pending title) for a bookclerkd node to run, and waits for them. `--dry-run` only prints planned keys, here

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

Search the library index

```text
bookclerk library search [OPTIONS] <QUERY>
```

Arguments:

- `<QUERY>`: Lucene-style query string

Options:

- `-n, --limit <LIMIT>`: Max results (0 = all) (default `10`)
- `-b, --bare`: Print ASINs only
- `--rebuild-index`: Rebuild the search index before querying
- `--filter <FILTER>`: Run a saved quick filter by name

### `bookclerk library export`

Export library rows as CSV / JSON / XLSX (alias of `export library`)

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

### `bookclerk library list`

List books in the local library DB

```text
bookclerk library list [OPTIONS]
```

Options:

- `--account <ACCOUNT>`: Limit listed rows to this account id, stem, or nickname
- `--status <STATUS>`: Filter by acquire status (`acquired`, `not_downloaded`, …)

### `bookclerk library convert`

Convert acquired m4b/m4a to mp3

```text
bookclerk library convert [OPTIONS] [ASIN]...
```

Arguments:

- `[ASIN]...`: Title ids to convert; empty means all acquired titles matching the account filter

Options:

- `--account <ACCOUNT>`: Limit conversion to this account id, stem, or nickname
- `-f, --force`: Re-encode even when an MP3 already exists in storage

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

## `bookclerk discover`

Recommendations, embeddings, listening sync, and title requests

```text
bookclerk discover [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk discover recommend`](#bookclerk-discover-recommend): Rebuild works graph, optionally enrich + embed, then print recommendations
- [`bookclerk discover rebuild-works`](#bookclerk-discover-rebuild-works): Rebuild the works graph from ownership rows
- [`bookclerk discover enrich-openlibrary`](#bookclerk-discover-enrich-openlibrary): Run Open Library enrichment for metadata gaps
- [`bookclerk discover embed`](#bookclerk-discover-embed): Embed dirty works
- [`bookclerk discover sync-listening`](#bookclerk-discover-sync-listening): Sync listening progress from all capable integrations into the library DB
- [`bookclerk discover wishlist`](#bookclerk-discover-wishlist): Personal wishlist helpers (also feed the shared global queue)

### `bookclerk discover recommend`

Rebuild works graph, optionally enrich + embed, then print recommendations

```text
bookclerk discover recommend [OPTIONS]
```

Options:

- `-n, --limit <LIMIT>`: Max recommendations to print
- `--no-purchase-hints`: Skip purchase-hint HTTP lookups
- `--user <USER>`: Filter listening signals to one external user id (any listening integration)
- `--no-listening`: Ignore listening_progress entirely (owned-library taste only)
- `--listening-provider <ID>`: Only use listening rows from these integration ids (repeatable)

### `bookclerk discover rebuild-works`

Rebuild the works graph from ownership rows

```text
bookclerk discover rebuild-works [OPTIONS]
```

### `bookclerk discover enrich-openlibrary`

Run Open Library enrichment for metadata gaps

```text
bookclerk discover enrich-openlibrary [OPTIONS]
```

### `bookclerk discover embed`

Embed dirty works

```text
bookclerk discover embed [OPTIONS]
```

Options:

- `--hash`: Force the local-hash embedder (no ONNX download)

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
- [`bookclerk plugins install`](#bookclerk-plugins-install): Install a plugin version into the cluster from a source-qualified coordinate or a local archive. The first installed version (or `--activate`) becomes the active version every node runs; deploy it with `plugins deploy`
- [`bookclerk plugins update`](#bookclerk-plugins-update): Install and activate a newer version of an installed plugin (or update a staged package by its receipt)
- [`bookclerk plugins rollback`](#bookclerk-plugins-rollback): Activate an earlier installed version of a plugin
- [`bookclerk plugins versions`](#bookclerk-plugins-versions): List installed versions (the active one is starred)
- [`bookclerk plugins uninstall`](#bookclerk-plugins-uninstall): Uninstall a plugin: clear its active version and undeploy it from every node (or remove a staged package directory)
- [`bookclerk plugins info`](#bookclerk-plugins-info): Show details for one discovered plugin (local + receipt)
- [`bookclerk plugins diagnose`](#bookclerk-plugins-diagnose): Run diagnose probes for one (or all) plugins that support it
- [`bookclerk plugins doctor`](#bookclerk-plugins-doctor): Check install integrity, target, jail, and describe/health status
- [`bookclerk plugins deploy`](#bookclerk-plugins-deploy): Deploy a plugin instance. With no flags it runs on this node (or on `--node`) by hand: the instance's deployment there is `present`. With `--replicas` the cluster scheduler (one daemon, under a cluster lease) places it on that many nodes carrying every `--on` label and writes the deployments itself; `--manual` hands those deployments back to be managed by hand
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

Install a plugin version into the cluster from a source-qualified coordinate or a local archive. The first installed version (or `--activate`) becomes the active version every node runs; deploy it with `plugins deploy`

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

Deploy a plugin instance. With no flags it runs on this node (or on `--node`) by hand: the instance's deployment there is `present`. With `--replicas` the cluster scheduler (one daemon, under a cluster lease) places it on that many nodes carrying every `--on` label and writes the deployments itself; `--manual` hands those deployments back to be managed by hand.

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
- `[VALUE]`: Value text (`true`/`false`, a number, or a string)

Options:

- `--secret`: Seal a secret read from stdin

#### `bookclerk plugins config unset`

Unset one setting so the schema default applies, or drop a secret

```text
bookclerk plugins config unset [OPTIONS] <INSTANCE> <KEY>
```

Arguments:

- `<INSTANCE>`: Plugin alias, PluginKey, or plugin instance id
- `<KEY>`: Setting key

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
- [`bookclerk config master-key`](#bookclerk-config-master-key): Inspect or wrap `{files_dir}/master.key` (BCK1 ↔ BCK2)
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

Inspect or wrap `{files_dir}/master.key` (BCK1 ↔ BCK2)

```text
bookclerk config master-key [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk config master-key status`](#bookclerk-config-master-key-status): Show whether `master.key` exists and if it is raw (BCK1) or wrapped (BCK2)
- [`bookclerk config master-key wrap`](#bookclerk-config-master-key-wrap): Wrap a BCK1 `master.key` with a passphrase (no-op unlock if already BCK2)

#### `bookclerk config master-key status`

Show whether `master.key` exists and if it is raw (BCK1) or wrapped (BCK2)

```text
bookclerk config master-key status [OPTIONS]
```

#### `bookclerk config master-key wrap`

Wrap a BCK1 `master.key` with a passphrase (no-op unlock if already BCK2).

Password from `BOOKCLERK_AUTH_PASSWORD` or `[auth].password` — never argv.

```text
bookclerk config master-key wrap [OPTIONS]
```

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

## `bookclerk export`

Export library data, backups, or Libation-compatible files

```text
bookclerk export [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk export native`](#bookclerk-export-native): Write a portable Bookclerk `.tar.gz` backup of the files directory
- [`bookclerk export libation`](#bookclerk-export-libation): Export Settings.json, AccountsSettings.json, and LibationContext.db
- [`bookclerk export library`](#bookclerk-export-library): Export library rows as CSV / JSON / XLSX
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

### `bookclerk export library`

Export library rows as CSV / JSON / XLSX

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

## `bookclerk clusters`

Owner clusters this install hosts for, and consent to their plugins

```text
bookclerk clusters [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk clusters list`](#bookclerk-clusters-list): The owner clusters this install is enrolled in as a host
- [`bookclerk clusters join`](#bookclerk-clusters-join): Enroll this install as a host (a branch) of an owner library
- [`bookclerk clusters leave`](#bookclerk-clusters-leave): Leave an owner cluster: the owner is told (and revokes this node when it hears), the link stops being dialed and its node keys are deleted
- [`bookclerk clusters consents`](#bookclerk-clusters-consents): What each owner cluster asks to run here, and whether it may
- [`bookclerk clusters approve`](#bookclerk-clusters-approve): Approve an owner plugin's request to run here, optionally narrowed. Name the request you reviewed by the digest `clusters consents` shows (it covers the version, the grant, the archive URL, the archive digest and the payload digest); it is refused when the cluster has changed the request since. Only what the request asks for can be approved; a later version that asks for more needs approval again
- [`bookclerk clusters scan`](#bookclerk-clusters-scan): Ask an owner cluster to scan its library for this node's owner
- [`bookclerk clusters revoke`](#bookclerk-clusters-revoke): Withdraw an approval: held jobs of the plugin are cancelled and given back, and nothing of it is claimed until this host's operator approves it again (a platform plugin is not approved automatically)

### `bookclerk clusters list`

The owner clusters this install is enrolled in as a host

```text
bookclerk clusters list [OPTIONS]
```

### `bookclerk clusters join`

Enroll this install as a host (a branch) of an owner library.

With `--url`, prints a sign-in link (and opens a browser when it can): a user of that library signs in and approves, and the answer comes back to a loopback listener this command opens. Without it, reads a branch invite (`https://<owner>/enroll#bcn1...`) from stdin, or prompts for it; an invite is never taken from the command line.

```text
bookclerk clusters join [OPTIONS]
```

Options:

- `--url <URL>`: The owner library's address (`https://library.example`)
- `--name <NAME>`: The name the owner sees for this branch (default: host name)
- `--fingerprint <FINGERPRINT>`: The library's certificate fingerprint as Settings → Branches shows it (a `sha256:` prefix is accepted), confirming a self-signed library without a prompt
- `--as <JOIN_AS>`: How to join: `branch` (default; keeps this install's own database) or `clerk` (shares the library's database and key; only an empty install, and only when an Owner of the library chooses it after a fresh second factor) (one of `branch`, `clerk`)

### `bookclerk clusters leave`

Leave an owner cluster: the owner is told (and revokes this node when it hears), the link stops being dialed and its node keys are deleted

```text
bookclerk clusters leave [OPTIONS]
```

Options:

- `--cluster <CLUSTER>`: Owner cluster id (default: the only enrolled cluster)

### `bookclerk clusters consents`

What each owner cluster asks to run here, and whether it may

```text
bookclerk clusters consents [OPTIONS]
```

Options:

- `--cluster <CLUSTER>`: Only this owner cluster

### `bookclerk clusters approve`

Approve an owner plugin's request to run here, optionally narrowed. Name the request you reviewed by the digest `clusters consents` shows (it covers the version, the grant, the archive URL, the archive digest and the payload digest); it is refused when the cluster has changed the request since. Only what the request asks for can be approved; a later version that asks for more needs approval again

```text
bookclerk clusters approve [OPTIONS] --request <DIGEST> <PLUGIN>
```

Arguments:

- `<PLUGIN>`: Canonical plugin key

Options:

- `--cluster <CLUSTER>`: Owner cluster id (default: the only enrolled cluster)
- `--request <DIGEST>`: The `request_digest` of the request you reviewed, from `clusters consents` (refused when the cluster now asks for anything else) (required)
- `--deny-domain <DOMAIN>`: Take a domain out of the request. Repeatable
- `--deny-binding <BINDING>`: Take a binding out of the request (`secrets`, `kv`, ...). Repeatable
- `--deny-network`: Deny all network access

### `bookclerk clusters scan`

Ask an owner cluster to scan its library for this node's owner.

Goes through this install's running daemon, over its link to the owner. The owner plans it as that user: an Owner or Administrator scanning every account gets one job per account, spread over the cluster's nodes and hosts; anyone else scans only their own accounts. Waits for the jobs unless `--no-wait`.

```text
bookclerk clusters scan [OPTIONS]
```

Options:

- `--cluster <CLUSTER>`: Owner cluster id (default: the only enrolled cluster)
- `--account <ACCOUNT>`: Scan only this store account of the owner cluster
- `--instance <INSTANCE>`: Pin the scan to one owner plugin instance
- `--no-wait`: Print the queued job ids without waiting for them

### `bookclerk clusters revoke`

Withdraw an approval: held jobs of the plugin are cancelled and given back, and nothing of it is claimed until this host's operator approves it again (a platform plugin is not approved automatically)

```text
bookclerk clusters revoke [OPTIONS] <PLUGIN>
```

Arguments:

- `<PLUGIN>`: Canonical plugin key

Options:

- `--cluster <CLUSTER>`: Owner cluster id (default: the only enrolled cluster)

## `bookclerk nodes`

Cluster nodes: liveness, capacity, budgets, and placement labels

```text
bookclerk nodes [OPTIONS] <COMMAND>
```

Commands:

- [`bookclerk nodes list`](#bookclerk-nodes-list): Every registered node: liveness, artifact target, and labels
- [`bookclerk nodes set`](#bookclerk-nodes-set): Set a node's scheduling budget or drain it. Applies at that node's next heartbeat
- [`bookclerk nodes label`](#bookclerk-nodes-label): Add or remove placement labels on a node (`nas`, `zone=eu`)
- [`bookclerk nodes owner`](#bookclerk-nodes-owner): Assign a node to the user who administers it, or clear its owner. The owner (any active user) may then change the node's budgets, drain switch, and name. Work the node is trusted with still follows the owner's role
- [`bookclerk nodes invite`](#bookclerk-nodes-invite): Mint a single-use link that enrolls one host node for a user. The link (with its secret in the URL fragment) is printed once
- [`bookclerk nodes invites`](#bookclerk-nodes-invites): List or revoke enrollment links
- [`bookclerk nodes revoke`](#bookclerk-nodes-revoke): Revoke an enrolled host node's credential. Its connection closes and it receives no more work

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

Mint a single-use link that enrolls one host node for a user. The link (with its secret in the URL fragment) is printed once

```text
bookclerk nodes invite [OPTIONS] --user <USER>
```

Options:

- `--user <USER>`: Login name or user id of the user who will own the host node (required)
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

Revoke an enrolled host node's credential. Its connection closes and it receives no more work

```text
bookclerk nodes revoke [OPTIONS] <NODE>
```

Arguments:

- `<NODE>`: Host node id

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

## `bookclerk version`

Print version information

```text
bookclerk version [OPTIONS]
```
