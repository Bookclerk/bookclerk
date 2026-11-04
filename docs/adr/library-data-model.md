# ADR: Library data model (works, expressions, editions, holdings)

- **Status:** Accepted; not yet implemented (see [Status of the work](#status-of-the-work))
- **Date:** 2026-11-04
- **Related:** [Schema versioning](schema-versioning.md),
  [SQL database contract](sql-database-contract.md),
  [Plugin capabilities v3](plugin-capabilities-v3.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md),
  [Control plane](control-plane.md)

## Context

The library stores one `books` row per store product per account, keyed
`(plugin_key, account_id, product_id)`. That row mixes four things:

- bibliographic metadata (title, authors, narrators, series, publisher);
- the store listing (marketplace, categories, store ratings);
- the listener's state (`is_finished`, `tags`);
- acquisition (`acquire_status`, `storage_key`, `pdf_storage_key`).

Authors and narrators are comma-separated text and `content_kind` is a free
string. A `works` table and a `work_editions` join exist, but they are
rebuilt from ASIN or ISBN alone and `works` still carries narrators.

This shape causes four problems:

1. **Mixed media.** Audible and OpenLibrary return podcasts, print and
   ebook editions. `ScanBook.contentKind` is optional text and `CatalogHit`
   has no kind at all, so everything looks like an audiobook.
2. **Record clashes in a cluster.** Two accounts that own the same edition
   are two rows that acquire to the same naming-template path. Branch titles
   live in `branch_titles`, keyed `(provenance, store, product_id)`, and only
   match a library row with the same store and product id, so an Audible
   copy and a Libro.fm copy of one edition never meet. `title_files` then
   refuses the upload ([#300](https://github.com/Bookclerk/bookclerk/issues/300),
   [#314](https://github.com/Bookclerk/bookclerk/issues/314),
   [#321](https://github.com/Bookclerk/bookclerk/issues/321)).
3. **Future media.** Transcripts, generated EPUBs and read-along
   ([#121](https://github.com/Bookclerk/bookclerk/issues/121),
   [#125](https://github.com/Bookclerk/bookclerk/issues/125)) need files with
   roles, not two storage-key columns.
4. **Many under one book.** A work can have several narrations, translations,
   abridgements, publishers and store releases. None of that has a place.

The host schema is still the unreleased development pack
([schema versioning](schema-versioning.md)), and `ScanBook` and `CatalogHit`
in `plugin.capnp` are append-only. Both get more expensive to change after the
first release, so the model changes now.

## Decision

### 1. Three bibliographic levels, following FRBR

The model follows the IFLA *Functional Requirements for Bibliographic
Records* (FRBR) entity model, and its successor the IFLA Library Reference
Model, which keeps the same four Group 1 entities. Using the established
library model, rather than one of our own, keeps Bookclerk's vocabulary
recognisable to librarians and cataloguers, lines its data up with sources
that already speak FRBR (OpenLibrary's works and editions), and leaves room
for features that model already describes.

| FRBR entity | Bookclerk | Notes |
| --- | --- | --- |
| Work | `works` | |
| Expression | `expressions` | |
| Manifestation | `editions` | "Edition" is the word people use for a manifestation; the docs and UI say edition |
| Item | `holdings` and `edition_files` | A holding is an exemplar someone owns; its files are the copy at a destination |
| Person, corporate body (Group 2) | `contributors` | |
| Responsibility relationships | `credits` with a role | Created by (work), realised by (expression), produced by (manifestation) |

| Level | One row is | Carries |
| --- | --- | --- |
| **Work** | A creative work, independent of any store or format | Title, `kind`, original language, first published, `parent_work_id`, author credits, series entries |
| **Expression** | One realisation of a work: the text, a narration, a translation, a dramatisation, an abridgement | Work, `kind`, language, abridgement, narrator, translator and cast credits |
| **Edition** | One release of an expression | Expression, `media_kind`, publisher credits, published date, duration or page count, cover, identifiers |

`works.kind` is a closed enum: `book`, `podcast_show`, `podcast_episode`,
`drama`. Podcasts use the parent link: the show is a work, each episode is a
work whose `parent_work_id` is the show, and each episode has its own
expression and edition. Naming's `podcast` and `podcastparent` conditions read
the parent link.

`expressions.kind` is a closed enum: `text`, `narration`, `translation`,
`dramatisation`, `podcast_audio`. Abridgement (`unknown`, `unabridged`,
`abridged`) lives on the expression, because an abridged text is a different
realisation of the work.

`editions.media_kind` is a closed enum: `audiobook`, `ebook`, `print`,
`podcast_audio`. Print and ebook editions are stored, not dropped: they show
in discover and their ISBNs help tie audio editions to a work. They are never
acquire candidates.

Each enum is a Rust enum matched exhaustively, a capnp enum on the wire, and a
`CHECK` constraint in SQL. A new variant is appended in all three at once.

### 2. Supporting tables

| Table | One row is | Unique key |
| --- | --- | --- |
| `edition_identifiers` | One external id of an edition: `scheme` (`asin`, `isbn13`, `olid`, or a store's own product scheme), `value`, `marketplace` (empty for global schemes) | `(scheme, value, marketplace)` |
| `contributors` | A person or organisation, with a sort name | Surrogate id |
| `credits` | A contributor's `role` on a work, expression or edition, with a position | `(target_kind, target_id, contributor_id, role)` |
| `series`, `series_entries` | A series and a work's place in it (position text plus a sortable decimal) | `(series_id, work_id)` |
| `store_listings` | What a store sells: `plugin_key`, `marketplace`, `product_id`, the edition, store ratings and price, a `metadata_json` snapshot, `observed_at` | `(plugin_key, marketplace, product_id)` |
| `holdings` | Ownership of a listing by an account, or by a branch (`node:<node_id>` provenance), with entitlement (`purchase`, `plus`, `loan`, `free`) and `purchased_at` | `(listing_id, holder)` |
| `edition_files` | A file of an edition at a destination, with `role` (`audio`, `part`, `chapter`, `pdf`, `cover`, `sidecar`, `transcript`, `epub`), digest, quality and writer | `(destination, storage_key)` |
| `user_title_state` | One user's finished flag, rating, tags and progress for a work | `(user_id, work_id)` |
| `resolution_overrides` | An Administrator's manual merge or split, which the resolver obeys | `(subject_kind, subject_id)` |

Credit roles are a closed enum: `author`, `narrator`, `translator`, `editor`,
`illustrator`, `publisher`, `host`, `cast`, `adaptor`.

These tables replace `books`, `works`, `work_editions`, `title_files` and
`branch_titles`. `listening_progress`, `title_requests`, `ignored_titles` and
`embeddings` point at works or editions instead of `book_uuid`.

### 3. Files belong to editions

Acquisition is per edition, not per holding: two holdings of one edition
acquire once. `edition_files` decides what an acquire or a host upload may
write at a key that is already recorded:

| Existing key holds | Incoming file | Outcome |
| --- | --- | --- |
| Same edition, same role | Better quality (higher bitrate or a lossless source) | Replace |
| Same edition, same role | Same or worse quality | Keep; the upload is refused |
| Same edition, another role | A sidecar, PDF or transcript | Add at its own key |
| Another edition | Anything | Disambiguate: naming appends an edition discriminator to the path |

This is the rule [#300](https://github.com/Bookclerk/bookclerk/issues/300)
asks for. Keys a title will write are derived from its edition and the
download options before an upload is granted.

### 4. One resolver, in the host, fed by plugins

Storefronts and metadata providers describe what they see; the host decides
identity. A sans-IO resolver maps each incoming listing to an edition, an
expression and a work:

1. **Edition** by identifier. Any shared identifier means the same edition.
   Two identifiers that point at different editions are never merged
   automatically; the conflict is recorded for an Administrator.
2. **Expression** within the work: same kind, language, abridgement and
   narrator or translator credits.
3. **Work** by a shared work-level identifier (an OpenLibrary work id, for
   example), else by the existing `bookclerk-enrich` match score above a
   confidence floor, else a new work.
4. `resolution_overrides` win over every automatic step.

Resolution runs on guest-supplied identifiers and credits, so it is an
untrusted-input boundary. It ships with a `cargo-fuzz` target, a seed corpus
and a replay test, with an oracle that checks: one identifier never maps to
two editions, editions with conflicting identifiers are never merged, and
resolving the same input twice changes nothing
([fuzzing ADR](untrusted-input-fuzzing.md)).

Each link is written as one atomic batch, never as a read-modify-write across
statements, because D1 has no interactive transactions
([#320](https://github.com/Bookclerk/bookclerk/issues/320)). All DDL and
queries go through SQL-v1.

### 5. The plugin ABI grows, it is not replaced

`ScanBook` and `CatalogHit` gain new ordinals only:

- `mediaKind :MediaKind` and `workKind :WorkKind`;
- `identifiers :List(Identifier)` (scheme, value, marketplace);
- `credits :List(Credit)` (name, role, level, position);
- `seriesEntries :List(SeriesEntry)`;
- `parentProductId` for podcast episodes.

The flat text fields (`authors`, `narrators`, `asin`, `isbn`, `contentKind`,
`series`, `seriesIndex`) stay on the wire, documented as deprecated. The host
prefers the structured fields and falls back to the flat ones. The TS and
Python SDKs regenerate in the same pull request.

A new `metadata` entrypoint returns the same edition records for a lookup by
identifier or by title and author. OpenLibrary and Audnexus, which live in
`bookclerk-discover` and `bookclerk-enrich` today, move into plugins behind
it.

### 6. Events instead of rebuild passes

The resolver publishes `edition.linked`, `holding.added`, `holding.removed`
and `edition_files.written` on the `core.events` outbox. Integrations
(Audiobookshelf), embeddings and discover subscribe to them. The batch
`rebuild_works_from_library()` pass is removed.

### 7. Moving existing libraries: reset, rescan, restore state

No in-place migration is written for an unreleased schema. Instead:

1. `bookclerk library export-state` writes what only Bookclerk knows: finished
   flags, tags, ignored titles, title requests, listening progress and saved
   filters, keyed by store, product id and identifiers.
2. The development reset applies the new schema, as
   [schema versioning](schema-versioning.md) already requires for a changed
   unreleased checksum.
3. A rescan of each account fills listings, holdings and the bibliographic
   tables.
4. Storage matching re-links files already at each destination into
   `edition_files`, so nothing is downloaded again.
5. `bookclerk library import-state` re-applies the export by identifier.

The Libation importer in `bookclerk-migrate` writes the new tables directly.

### 8. Web API

`/api/library/books` and `/api/library/books/{uuid}` are removed, with no
compatibility view. They are replaced by `/api/library/works`,
`/api/library/editions` and `/api/library/holdings`, with typed OpenAPI
schemas and examples, and media-kind filters. Errors name the entity that was
not found ([#559](https://github.com/Bookclerk/bookclerk/issues/559)).

## Consequences

- Two stores or two accounts selling one edition meet in one place, so the
  cluster can deduplicate acquires and branch deliveries by edition.
- Mixed media is typed end to end, and print and ebook editions can show in
  discover without ever being acquired.
- Per-user state means members sharing a library no longer overwrite each
  other's finished flags, ratings or tags.
- Identity depends on identifier quality. A store that reuses or omits
  identifiers produces new editions rather than wrong merges; Administrators
  fix the rest with overrides.
- Three levels add a join to most title queries. The list views read a
  denormalised page index refreshed by the events above, not the full join.
- Every development library is reset once. Backups taken before the change
  can't be restored afterwards.
- Storefront plugins need an update to fill the structured fields, but keep
  working unchanged in the meantime through the flat-field fallback.

## Status of the work

Planned as separate pull requests, in this order:

1. This ADR.
2. ABI: the new capnp fields and enums, both SDKs, and the four first-party
   storefronts filling them.
3. Schema and resolver, with its fuzz target, plus `export-state` and
   `import-state`.
4. Acquire and files: per-edition acquisition, `edition_files`, and the
   naming discriminator.
5. Cluster: branch titles become holdings; node frames carry identifiers and
   media kind.
6. Web API and UI.
7. The `metadata` entrypoint, with OpenLibrary and Audnexus moved into plugins.

User-facing pages (the glossary, *Where books go*, *Branches*, the plugin ABI
reference) change with the step that changes their behaviour.
