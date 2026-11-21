# ADR: Metadata claims and two-way sync with files

- **Status:** Accepted; nothing implemented yet (see [Order of work](#order-of-work)).
  Section 4 is amended by [Uploads and ingest batches](uploads-and-ingest.md):
  an imported book is held by a patron holding.
- **Date:** 2026-11-17
- **Related:** [Library data model](library-data-model.md),
  [Event delivery model](event-delivery-model.md),
  [Plugin capabilities v3](plugin-capabilities-v3.md),
  [SQL database contract](sql-database-contract.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

Bookclerk writes metadata into the files it acquires and never reads it
back. Other tools do both, each in its own way:

- **Audiobookshelf** reads folder names, embedded tags, the first `*.nfo`
  (plain `Key: value` text), `desc.txt` and `reader.txt`, the first `*.opf`
  and a file named exactly `metadata.json`, in that order per library. A
  later source overwrites an earlier one field by field, and nothing records
  which source set a field. By default it keeps its own `metadata.json`
  outside the book folder. A scan, a bulk match and a manual match are three
  code paths with different sources and overwrite rules, so the same book
  can end up with three different answers.
- **Libation** writes the store's catalog record to
  `<book name>.metadata.json` once at download and never reads it again.
  Audiobookshelf ignores that file because of its name.
- **Jellyfin** reads OPF for books and embedded tags for audiobooks. It has
  no NFO reader for either; its Kodi-style `album.nfo` applies to music
  libraries.
- **Tag editors** rewrite files in place, sometimes thousands at a time.

Inside Bookclerk, three things stood in the way of reading metadata back:

- A field holds one value with no record of where it came from. Metadata
  providers fill gaps in plugin registration order, and a filled field is
  never revisited.
- The work-level matcher uses one exact key, the cleaned title plus the
  author credits, and the first listing to arrive gives the work its title
  and cover. A listing that is one part of a production ("1 of 3") drops
  the part from its key and can therefore name the whole work.
- The storage interface reports a key and a size for each object, so the
  host cannot tell that a file changed, and it has no move operation.

No outside numbering system identifies works and editions both openly and
completely. ISBNs have no open registry and some stores do not return them.
Community catalogs disagree with each other and are sometimes wrong about
which work an edition belongs to. Bookclerk is expected to run its own
catalog service later.

## Decision

### 1. Every value is a claim, and one resolver decides

A claim is one source's value for one field of a work, an expression or an
edition, in one language (a BCP 47 tag, or unknown). Sources are a store
listing, a named metadata provider, a file's embedded tags, a sidecar file,
an integration, a person's edit in Bookclerk and the folder path.

Fields keep the home level the
[data model](library-data-model.md) gives them. A claim read from a file is
about that file's edition; for a work-level field it becomes a suggestion on
the work unless the work has only that edition.

One resolver decides each field, in this order:

1. A value an Administrator pinned.
2. The newest edit made in Bookclerk.
3. The newest outside edit: a change found in a file Bookclerk did not
   write, or an edit made in an integrated tool.
4. The best machine source, by a ranking that is set per field and names
   providers. The default ranks the edition's own listing first, then a
   provider found by a strong identifier, then files as first imported, then
   a provider found by a weak identifier or a search, then the folder path.
5. An empty value never replaces a filled one unless a person cleared the
   field.

"Newest" is the highest revision number the database gave the subject,
never a clock, because branches' clocks differ and file times can be set.
Two edits of one field from the same starting value are a conflict: the
current value stays until an Administrator chooses.

Every way metadata arrives (a scan, a bulk match, a manual match, a store
refresh, a file change, an edit in an integrated tool) adds claims and runs
this resolver. None of them writes a field directly. The resolver is a pure
function of a subject's claims, so the order in which sources arrived cannot
change the result.

Rules that keep this affordable:

- The resolver's result is written to the existing work and edition
  columns. List pages, search and Discover read those columns as before, and
  claims are read only when a subject's claims change.
- There is one current claim per subject, field, source and language, and a
  bounded history of earlier values per field.
- Long values (descriptions, chapter lists) are stored once, chunked and
  keyed by digest, because the smallest supported database allows 1,024
  bytes in a cell ([database limits](../plugins/database-limits.md)).
- Store listings stay in `store_listings` and are read as claims.
- When one source changes many editions in one scan, the set is held as a
  single entry for one decision, so a bulk retag by a tool cannot silently
  override store data across a library.

### 2. Works are grouped by weighing evidence

The work-level step of the
[data model's resolver](library-data-model.md#4-one-resolver-in-the-host-fed-by-plugins)
changes from one exact key to weighed evidence. The edition and expression
steps keep their rules: a strong identifier names an edition, and a weak one
(an ISBN) proposes an edition that is accepted when media kind, language,
abridgement and narrator credits are compatible. Running time joins that
compatibility check.

| Evidence | Effect |
| --- | --- |
| An Administrator's merge, split or pin | Decides; never undone automatically |
| A store's own link between formats or parts | Joins |
| Two providers placing the editions' ISBNs in one work | Joins |
| A shared author and a core title that matches one of the work's known titles | Joins |
| A shared author and the same series and position | Joins when nothing speaks against it |
| The same ISBN on an incompatible expression | Same work, another edition |
| One provider's work lookup alone, a provider's original title, a shared folder | Suggests |
| Publication year, running time, publisher | Tips a close call |
| A different series position, a different primary author, a marker of a derived book (a summary or study guide) | Blocks an automatic join |
| A collection, omnibus or box set | Its own work, which points at the works it contains |

- A shared author is required for any automatic join. Adaptors, translators
  and narrators do not count as authors here.
- A title is parsed into a core and classified extras: subtitle, series
  decoration, part designation and kind markers. Matching uses the core.
  The extras become fields; they are not discarded.
- A work keeps every title its editions carry, with the language of each.
- A dramatisation or a part of a production stays in the work it adapts and
  does not name it: the work's title and cover are resolved from claims, and
  a whole narration or text edition ranks above a part.
- The same evidence gives the same groups. Deciding links are applied
  first, then the remaining joins in a fixed order (strongest first, then by
  a stable key), skipping a join that would put a blocked pair in one work.
- The resolver may merge two works when new evidence joins them. The older
  work keeps its id, the other id redirects, and each person's state for the
  two is combined. It never splits a work on its own; a split is suggested.
- When the evidence neither joins nor blocks, the listing starts a new work
  and a "possibly the same work" entry is recorded for an Administrator.
- Candidates are found through indexed keys (identifier, author, core
  title, series), never by comparing against every work, and matching runs
  as a claimed job so two branches cannot create one work twice.
- Each join records the evidence that made it.

### 3. Ids stay local; a catalog assigns shared numbers later

Work and edition ids stay as they are: made by the instance, used in links,
per-person state and the `<call number>` in file names, and never changed by
metadata. Works and editions gain an empty `catalog_id` slot for a number a
future Bookclerk catalog service assigns. That number carries no meaning, so
correcting a match never changes it.

Outside numbers (ISBN, ASIN, a store's product id, keys from community
catalogs) are stored as evidence under their scheme. No id is computed from
them, and no outside catalog is treated as the authority. The catalog
service is a separate decision.

### 4. Files on storage are read, whatever Bookclerk writes

A paged scan lists a destination, groups objects into candidate books, reads
embedded tags and any metadata file a dialect recognises, and identifies
each candidate: by Bookclerk's own `<call number>` in the path, then by a
strong identifier, then through the matcher with file tags as lower-trust
evidence. An imported book that no store account owns gets its files, an
acquired state and a patron holding for the person who added it (amended
by [Uploads and ingest batches](uploads-and-ingest.md)). A book that cannot be identified still plays
and is marked for an Administrator.

Reading a third-party metadata file does not depend on whether Bookclerk
writes that file.

### 5. Changes are detected in layers

| Layer | What | Role |
| --- | --- | --- |
| Stat signature | Size, modification time, inode and change time locally; ETag or version id on object stores | Says when to look |
| Metadata hash | The whole file for a sidecar; only the metadata region for media | Says whether the metadata bytes changed |
| Value hash | A hash of the parsed, normalised values, carrying the normaliser's version | Decides whether anything changed |
| Write receipt | The signature and hashes Bookclerk recorded after its own write | Keeps Bookclerk's writes from reading back as edits |

A scheduled, paged listing compared with stored signatures is the baseline
and works on every backend. A storage plugin may also offer a change feed:
the `local` plugin may stay awake to watch its root, as a declared exception
to [plugins sleeping when idle](event-delivery-model.md), and switches the
watcher off where the filesystem delivers no events; an object-store plugin
may drain the provider's notification queue. A feed is a hint, and the
listing catches what a feed missed.

Only the fields that moved since the base (what Bookclerk last wrote or
read) become outside-edit claims. An outside edit applies on its own when
nothing else changed that field.

### 6. Writing back is a three-way merge

| The file changed | Bookclerk changed | Result |
| --- | --- | --- |
| no | no | Nothing |
| yes | no | The file's value becomes an outside-edit claim |
| no | yes | Bookclerk's value is written |
| yes, to the same value | yes, to the same value | The new base is recorded |
| yes | yes, differently | A conflict is held for an Administrator |

- A write happens only if the file still matches the base: a conditional
  write where the store supports one, otherwise check, write and check
  again. A lost race becomes an ordinary outside change.
- A writer receives the existing bytes and changes only the fields
  Bookclerk resolves. Unknown keys, atoms and frames are kept.
- Writes are jobs keyed by edition, revision and destination, delayed until
  the edition has been quiet for a short time.

Every metadata file output is off by default. An output that is on is kept
in the edition's folder and synced both ways. The outputs are
Audiobookshelf's `metadata.json`, its plain-text NFO, a Kodi-style
`album.nfo`, OPF, `desc.txt` and `reader.txt`, and CUE. The store's catalog
JSON is written once at acquire, as before, and otherwise only read.
Re-embedding tags after an edit is a separate switch, off by default,
because it rewrites whole media files.

Audiobookshelf is synced through its integration plugin, which pushes
resolved changes over the tool's API at a limited rate and reports edits
made there. Bookclerk does not write `metadata.json` for a destination that
an Audiobookshelf integration syncs.

A dramatisation is written as its own field, `EXPRESSION_KIND`, never as a
genre. Storefront plugins detect it from structured fields first and then
from the bracketed phrase in a title, through one shared helper that the
importer also uses.

### 7. Formats live in confined components

- Embedded tags are read and written by `bookclerk-media-worker` in a
  long-lived confined mode, with no in-process fallback.
- Each sidecar format is a dialect behind a new `metadataFiles` plugin
  entrypoint with `parse` and `render`. A dialect has no network or file
  grants: the host hands bytes in and out and chooses every path. A dialect
  declares the fields it carries, and a field a format cannot carry is
  neither written nor treated as cleared.
- Sidecars are size-capped before parsing, XML is parsed without entity
  expansion, archive expansion is bounded, and rendered output is capped.
- Every new parser and every new request or reply shape ships with a fuzz
  target, a seed corpus and a replay test
  ([fuzzing ADR](untrusted-input-fuzzing.md)).

### 8. Storage operations state intent

The host asks a storage plugin for what it wants done, and the backend
chooses how. Beside the required primitives, a plugin may implement `move`,
`moveMany`, `deleteMany`, `headMany`, a listing with hints, a change feed
and a conditional write.

1. Each optional operation is a capability the plugin declares in its
   manifest, as `storage.copy` is, so the host knows what a sleeping plugin
   offers. Staying awake to watch is declared the same way.
2. The host keeps one fallback per operation, built from the required
   primitives.
3. A reply says how far a hint was honoured, and a change feed says whether
   it is complete.
4. Long operations are paged and safe to repeat.
5. Options are structures that only gain fields.

### 9. Work is jobs; events only announce

Scanning, reading, reconciling, matching and writing back are jobs, each
over a page of items. One event, `edition_metadata_changed`, announces a
resolved change with the edition, the changed fields and a revision. It is
emitted once per edition per quiet period and only when a value really
changed, and a consumer that slept past its retention period catches up by
revision ([event delivery model](event-delivery-model.md)).

## Consequences

- Every value can say where it came from, and a better source added later
  improves existing books without new requests.
- A scan, a match and an edit can no longer disagree, because they share
  one resolver.
- Two instances may group the same books differently until a catalog
  assigns shared numbers; grouping stays a local decision until then.
- Some editions that one exact key used to join now wait as suggestions.
  That is the chosen trade: a wrong automatic merge is harder to notice and
  undo than a missed one.
- The database gains three tables (claims, long values, file sync state)
  and extends `resolution_overrides`, `resolution_conflicts` and the storage
  scan index. All of it goes through SQL-v1 and is tested under every
  database profile.
- `plugin.capnp` gains fields and one entrypoint, appended with the SDKs:
  storage signatures and operations, an open form of `IdentifierScheme`,
  part number and count, format and part links, an original title, and
  `metadataFiles`.
- The `local` storage plugin becomes the first plugin allowed to stay awake
  while idle. Its resident cost is measured before the watcher is on by
  default.
- Sizes and durations for large libraries are estimates until each piece is
  measured on the smallest supported host.

## Order of work

1. This record.
2. The work matcher, with the listing fields it needs.
3. The claims tables and the resolver, writing into today's columns.
4. The storage interface: signatures, conditional writes, the optional
   operations and change feeds.
5. Tag reading in the media worker, then the paged import.
6. The `metadataFiles` entrypoint and the first-party dialects.
7. Change detection, reconciling and the review inbox.
8. Writing back, and the Audiobookshelf channel.
