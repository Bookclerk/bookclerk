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
([schema versioning](schema-versioning.md)), and no plugin has been built
against a released `plugin.capnp`. Both get more expensive to change after the
first release, when the schema freezes and the ABI becomes append-only, so the
model changes now.

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
| `work_identifiers` | One external id of a work (an OpenLibrary work id, for example), shared by all its expressions and editions | `(scheme, value)` |
| `edition_identifiers` | One external id of an edition: `scheme`, `value`, `marketplace` (empty for global schemes) and `strength` (see below) | `(scheme, value, marketplace)` for strong schemes only |
| `contributors` | A person or organisation, with a sort name | Surrogate id |
| `credits` | A contributor's `role` on a work, expression or edition, with a position | `(target_kind, target_id, contributor_id, role)` |
| `series`, `series_entries` | A series and a work's place in it (position text plus a sortable decimal) | `(series_id, work_id)` |
| `store_listings` | What a store sells: `plugin_key`, `marketplace`, `product_id`, the edition, store ratings and price, a `metadata_json` snapshot, `observed_at` | `(plugin_key, marketplace, product_id)` |
| `holdings` | Ownership of a listing by an account, or by a branch (`node:<node_id>` provenance), with entitlement (`purchase`, `plus`, `loan`, `free`) and `purchased_at` | `(listing_id, holder)` |
| `edition_files` | A file of an edition at a destination, with `role` (`audio`, `part`, `chapter`, `pdf`, `cover`, `sidecar`, `transcript`, `epub`), digest, quality and writer | `(destination, storage_key)` |
| `user_title_state` | One user's finished flag, rating, tags and progress for a work | `(user_id, work_id)` |
| `resolution_overrides` | An Administrator's manual merge or split, which the resolver obeys | `(subject_kind, subject_id)` |

Identifiers have a strength, fixed per scheme:

- **Strong** schemes name exactly one edition: a store's own product id in
  its marketplace, an Audible ASIN in its marketplace, an OpenLibrary edition
  id. They are unique.
- **Weak** schemes are shared in practice and only suggest a match: ISBN-10
  and ISBN-13 (Audible's abridged and unabridged products can carry one ISBN,
  as `bookclerk-enrich`'s `ISBN_MATCH_GAP_CLOSE` notes). They are indexed but
  not unique, and one weak value can sit on several editions.

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

1. **Edition** by a shared strong identifier. A weak identifier (an ISBN)
   proposes a candidate edition, which is accepted only when its expression
   is compatible (same kind, language, abridgement and narrator credits) and
   its media kind matches. Two strong identifiers that point at different
   editions are never merged automatically; the conflict is recorded for an
   Administrator.
2. **Expression** within the work: same kind, language, abridgement and
   narrator or translator credits. Any of these that the source left
   unspecified never matches a specified value, so missing data makes a new
   expression rather than a wrong merge.
3. **Work** by a shared `work_identifiers` row, else by a work-level matcher,
   else a new work. The work-level matcher compares normalised title and
   author credits only, reusing `bookclerk-enrich`'s text normalisation but
   not `calculate_match_confidence`: that score includes recording signals
   (duration, narrator) and treats any ten-character alphanumeric title as an
   ASIN. A matcher hit joins the work when the kind is the same and, for a
   podcast episode, the parent show is the same, so two shows by one host
   never share an episode called "Trailer". In the rare case that two
   existing works already match (an Administrator split them), the listing
   joins neither and the choice goes to the Administrator. A wrong
   automatic merge is undone by an override that splits the work.
4. `resolution_overrides` win over every automatic step.

Resolution runs on guest-supplied identifiers and credits, so it is an
untrusted-input boundary. It ships with a `cargo-fuzz` target, a seed corpus
and a replay test, with an oracle that checks: one strong identifier never
maps to two editions, a weak identifier alone never merges two editions with
incompatible expressions or media kinds, editions with conflicting strong
identifiers are never merged, works of different kinds are never merged,
and resolving the same input twice changes
nothing
([fuzzing ADR](untrusted-input-fuzzing.md)).

Each link is written as one atomic batch, never as a read-modify-write across
statements, because D1 has no interactive transactions
([#320](https://github.com/Bookclerk/bookclerk/issues/320)). All DDL and
queries go through SQL-v1.

### 5. The plugin ABI carries structured records

`ScanBook` and `CatalogHit` gain:

- `workKind :WorkKind`, `expressionKind :ExpressionKind` and
  `mediaKind :MediaKind`;
- `identifiers :List(Identifier)` (level, scheme, value, marketplace), where
  `level` says whether the id names the work or the edition, and so which
  identifier table it lands in;
- `credits :List(Credit)` (name, role, level, position);
- `seriesEntries :List(SeriesEntry)`;
- `parentProductId` for podcast episodes;
- on `ScanBook` only, `language` and `abridgement :Abridgement`, which
  `CatalogHit` already has.

Cap'n Proto reads an absent enum field as ordinal 0, so ordinal 0 is
always the value a guest gets when it says nothing. The schema is
unreleased, so there are no older guests to tell apart, and the new enums
follow the rule the schema already uses (`CatalogSort`, `Entrypoint`):

- Ordinal 0 is the common case: `book` for `WorkKind`, `audiobook` for
  `MediaKind`, `narration` for `ExpressionKind`, `edition` for the
  identifier level and `author` for the credit role. A guest that returns
  anything else sets the field, and the conformance tests check that each
  first-party plugin does.
- `unknown @0` is used only where a source can honestly not know the
  answer, and the resolver treats unknown differently from any known value.
  Today that is `Abridgement`, which already works this way.

After the v1 freeze the same rule decides any enum field added later:
ordinal 0 must then be the right answer for a guest built before the field
existed.

The flat text fields they replace (`authors`, `narrators`, `asin`, `isbn`,
`contentKind`, `series`, `seriesIndex`) are removed from both records, and
the host has no fallback to them. Nothing has been released against them,
so this is the one cheap moment to remove them; once v1 freezes, removed
fields would stay on the wire for good. The pull request that changes the
records also moves every first-party storefront to the structured fields
and regenerates the TS and Python SDKs. `plugin.capnp` is append-only from
the v1 freeze.

A new `metadata` entrypoint returns the same edition records for a lookup by
identifier or by title and author. OpenLibrary and Audnexus, which live in
`bookclerk-discover` and `bookclerk-enrich` today, move into plugins behind
it.

### 6. Events instead of rebuild passes

The resolver publishes these on the `core.events` outbox:

- `edition.linked` when a listing joins an edition, new or existing;
- `edition.updated` when a rescan or a metadata lookup changes an existing
  edition's or its work's metadata (title, cover, credits, series, media
  kind) without linking anything new;
- `holding.added` and `holding.removed`;
- `edition_files.written`.

Integrations (Audiobookshelf), embeddings, discover and the search index
subscribe to them, so a metadata-only change refreshes them as a new link
does. The batch `rebuild_works_from_library()` pass is removed.

### 7. Existing development libraries: reset

Bookclerk has no release, so no library holds anything that has to be
carried across. The schema step changes the unreleased checksum like any
other schema change. A development library then fails closed as
[schema versioning](schema-versioning.md) requires, and is recreated with
`cargo reset --yes`. No in-place migration, state export or partial rebuild
is written.

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
- Every storefront plugin changes in the same pull request as the records.
  A third-party plugin built against the development schema has to be
  rebuilt, which is acceptable before the first release.

## Status of the work

Planned as separate pull requests, in this order:

1. This ADR.
2. ABI: the new capnp fields and enums replace the flat ones, both SDKs,
   and the four first-party storefronts filling them. Until step 3 the host
   maps the structured fields onto the current `books` row.
3. Schema and resolver, with its fuzz target.
4. Acquire and files: per-edition acquisition, `edition_files`, and the
   naming discriminator.
5. Cluster: branch titles become holdings; node frames carry identifiers and
   media kind.
6. Web API and UI.
7. The `metadata` entrypoint, with OpenLibrary and Audnexus moved into plugins.

User-facing pages (the glossary, *Where books go*, *Branches*, the plugin ABI
reference) change with the step that changes their behaviour.
