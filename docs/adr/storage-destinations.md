# ADR: Storage destinations are plugin instances

- **Status:** Accepted (decisions made with the maintainer on 2026-11-18)
- **Date:** 2026-11-18
- **Related:** [Event delivery model](event-delivery-model.md),
  [Metadata claims and file sync](metadata-claims-and-file-sync.md),
  [Library data model](library-data-model.md),
  [Library catalog network](library-catalog-network.md)

## Context

Bookclerk writes acquired books to storage plugins. Until now the host knew
two of them by name: `local` and `s3`. It refused a second instance of
either, wrote every acquire to both when both were deployed, and treated
`local` as the only plugin with filesystem-style keys. A third storage
plugin could be installed but was never written to.

Operators want more than that. They add the same plugin several times (two
buckets, a NAS and a laptop folder), mirror books across destinations, or
send some books to one place and other books to another. First-party R2 and
B2 plugins are planned, and each brings its own way of reporting outside
changes: a pulled queue for R2 and signed webhooks for B2.

## Decision

### 1. A destination is a deployed storage plugin instance

Every deployed instance of a storage plugin is a destination, named by its
instance id and shown by its display name. Nothing in the host names a
storage plugin. A fresh library gets one `local` instance with no filter,
so it takes every book until the operator changes that.

### 2. Each destination chooses what it takes

A destination carries a naming template and an optional filter.

1. The template is a string in the host's key syntax, such as
   `{author}/{series}/{title}`.
2. The filter is built like a scope: tags for patron, storefront or
   account, media kind and similar fields. No filter takes every book.

There is no ordered rule list and no fallback destination.

### 3. The target set is fixed before an acquire starts

1. Before an acquire begins, the host evaluates every destination's
   filter. An empty set fails the job at once ("no destination accepts
   this book").
2. The download streams to every target in the set at the same time.
3. The job succeeds as soon as one target holds the complete files.
   Failed transfers and unavailable targets are retried in the
   background.
4. Each stored copy is recorded per destination instance, with the key
   it used and its state (stored, pending retry, failed). In the data
   model these are `edition_copies` rows keyed by edition and
   destination, beside the per-file `edition_files` rows; a holding
   remains a store entitlement
   ([library data model](library-data-model.md)).
5. Changing a template or filter affects new acquires. Moving existing
   files to match is a separate job the operator starts.

### 4. The host owns the key syntax; the plugin owns the translation

The host fills the template into a key of named segments. Each plugin
translates that key into a path or an object key, under its own limits
(case-insensitive filesystems, reserved characters, path length). The
plugin returns the key it used, which the holding stores, and reports a
collision instead of overwriting another book's file.

### 5. Change feeds are opportunistic

Every destination has a fallback for noticing outside changes: a paged
listing compared with the previous one, run as a scheduled job. A push
feed, when the operator turns it on and it is healthy, makes changes show
up sooner and lets the listing run rarely. A feed that fails, lags or
reports a gap hands over to the listing at once. The plugin reports which
feed it uses and its health.

### 6. Webhooks are a host-held entrypoint

A plugin may declare webhooks in its manifest. The host serves them under
an unguessable per-instance path, outside operator auth, on a library
whose public origin an outside service can reach over the Internet. Being
a core branch (reachable by its own branches, which may be on a LAN) is
not enough by itself, so the host checks reachability per plugin. Before
waking the guest, the host enforces a body cap, a rate limit and any
signature rule the manifest declares. By default it stores a verified
request, answers at once and wakes the guest to handle it, as it does for
socket messages. Without an Internet address there is no webhook URL, and
the plugin uses its fallback.

### 7. Cloud setup through the plugin

A storage or database plugin for a cloud service may offer a guided setup.
The host runs the browser sign-in and hands a short-lived grant to the
plugin's provisioning entrypoint. The plugin creates what it needs and
returns narrowly scoped credentials, which the host seals like any other
secret. Manual entry stays available.

The first grant is an API token the operator creates at the provider and
pastes; the browser sign-in comes later as a second grant kind. A plugin
declares its setup in `[provisioning]`: the provider, the grants it
accepts, where to create a token and which permissions it needs, the
settings it asks the operator for (`inputs`), the settings it may write
back (`outputs`), and whether it can remove what it created
(`deprovision`).

- The host opens a short-lived guest for the run with the instance's
  stored settings and secrets, the inputs laid over them, and calls only
  `Provisioning.provision`. Required settings may still be missing.
- The plugin returns non-secret settings, any number of credentials and a
  short summary. The host refuses an undeclared key, a malformed value,
  and a credential or the grant itself appearing in a setting or the
  summary, then stores the settings and inputs as one revision. A grant
  the plugin does not return as a credential is dropped.
- A run sees what an earlier run stored, so a second run updates and
  rotates rather than duplicating.
- The host seals the credentials before writing the revision that names
  them. A running instance picks up the new revision, and with it the new
  credentials, as it does any configuration change.
- `Provisioning.deprovision` removes what the setup created. The setup
  screen offers it before the instance is deleted; with a fresh grant when
  the stored credentials cannot do it.
- Provisioning runs under the instance's normal network grant. There is no
  provisioning-only egress: the hosts a setup calls are declared in
  `[capabilities.network]` and approved like any other.
- A database plugin has no library instance: it must work before the
  library opens, so it is configured in `config.toml`. Its setup runs from
  the CLI (`bookclerk config database setup d1`) with the configured
  settings and no secrets. The host writes the returned ids to
  `config.toml` and seals the returned token with the data-encryption key
  in `database.credential`, kept in the same store as the master key (the
  file, the OS keychain or `systemd-creds`), never in the environment or
  the database it opens.

## Consequences

- Destinations, stored files and reads key on the instance id. A read
  uses the file's destination and may fall back to another destination
  that stores the same file.
- `plugin.capnp` gains a webhook entrypoint and binding and a provisioning
  entrypoint, appended with the SDKs. The webhook route is a new
  untrusted-input boundary and ships with a fuzz target.
- R2 is reached through its S3-compatible endpoint (its only object API
  outside Workers) plus the Cloudflare API for queues and setup. B2 is
  reached through its native API, which is the only way to configure its
  event notifications.
- B2 has no atomic conditional write. Its plugin writes, then checks the
  name's versions and removes its own write when another one got in
  first, and declares that weaker guarantee.
- Operators who need every book in two places configure two destinations
  without filters; one failing destination never fails an acquire while
  another succeeds.

## Order of work

1. This record.
2. Destinations keyed by instance; templates, filters and the target-set
   check; simultaneous streaming; stored files and retries per
   destination; the plugin translating the host's key syntax.
3. The listing fallback and feed health.
4. The R2 plugin with its Cloudflare Queue feed.
5. The webhook entrypoint, binding and host route.
6. The B2 plugin on the native API, then its webhook feed.
7. The provisioning entrypoint and guided setup for R2, then D1.
