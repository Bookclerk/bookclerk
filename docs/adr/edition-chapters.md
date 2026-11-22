# ADR: Chapters belong to the edition and are resolved from claims

- **Status:** Accepted (decisions made with the maintainer on 2026-11-20).
  Decisions 1, 2, 4, 5, 6 and 8 are built; 3 is built at acquire only;
  7 is built for the playback manifest, the web player and `/player`,
  and the Audiobookshelf integration's push is planned
- **Date:** 2026-11-20
- **Related:** [Metadata claims and file sync](metadata-claims-and-file-sync.md),
  [Library data model](library-data-model.md),
  [Playback](playback.md),
  [Database limits](../plugins/database-limits.md)

## Context

Until this record, chapters existed only inside media files and in memory
during an acquire. Four sources already handed them to the host, and none
was kept:

- a storefront's `PlainFetch.chapters`, titles and starts with no depth.
  For some stores these are track markers, not the book's chapters;
- a metadata provider's `chapters` entrypoint, a tree with depths, the
  timeline's runtime and whether the starts are exact;
- Bookclerk's own processing, which shifts the provider's tree past the
  brand intro and snaps each start to the nearest speech onset;
- a file's embedded chapters and a sidecar file's chapter list.

Players need the list in the database: the web player to show chapters and
skip between them, and the Audiobookshelf-compatible `/player` routes to
answer with a `chapters` array. An Administrator also needs to fix a list
by hand, and the fix must outlast the next acquire or provider answer.

A chapter list is only true on the timeline it was measured on. The
stored copies one acquire writes (an M4B, an MP3, the original parts) are
cut from one packaged timeline, so they agree to within tens of
milliseconds. A second download from another store, or imported files,
may not.

## Decision

1. **Chapters belong to the edition.** Each claim records the runtime of
   the timeline it was measured on. A stored copy whose own runtime is far
   from the resolved list's will play chapters read from its own file;
   that guard needs the stored per-file probe and comes later.
2. **The claims resolver decides, unchanged.** A pinned claim wins, then
   the newest Administrator edit, then the newest outside edit, then the
   best machine source. For chapters the machine ranking is, best first:
   1. Bookclerk's aligned result (`processed`);
   2. a provider asked by a strong identifier whose starts are exact;
   3. the store's own chapters;
   4. a file's chapters as first imported;
   5. a provider whose starts are estimated, or one asked by a weak
      identifier;
   6. store markers of unknown kind;
   7. store track markers;
   8. one chapter per delivered part, named as the store named it.

   An empty list wins only from a person, who cleared the chapters on
   purpose. A list whose write is unfinished never wins. "Newest" is the
   highest revision the database gave the edition's chapter claims, never
   a clock.
3. **"Processed" is a derived claim.** It names the provider list it was
   aligned from by digest. Today it is written at acquire; re-aligning a
   stored copy when its input claim changes is a later job.
4. **Storefronts say what their markers are.** `PlainFetch` gains
   `chapterKind` (`unknown`, `chapters`, `tracks`), appended to
   `plugin.capnp` with the SDKs, so the host never guesses per store.
5. **Lists are stored once, by digest.** A list's chapters are rows of
   `chapter_list_entries` keyed by its SHA-256 digest, written in batches
   the database plugin accepts, and its `chapter_lists` head row is
   written last, so a list with a head row is complete. Claims
   (`metadata_claims`) and the resolved choice (`edition_chapters`) name a
   list by digest. A list holds at most 2,000 chapters with titles of at
   most 512 bytes and depths of at most 7.
6. **The claims tables are built now, with chapters as the only field.**
   Chapters are edition-only and need no work matcher, so they come before
   the rest of the claims work.
7. **Players read the database.** The playback manifest and `/player`
   serve the resolved tree, `/player` flattened in pre-order with each end
   at the next start, as Audiobookshelf does. The Audiobookshelf
   integration later pushes resolved chapters and reports chapter edits
   made there as outside-edit claims.
8. **A change is announced, not written into files.** When the list an
   edition plays changes, the host emits `edition_updated` with
   `fields: ["chapters"]`. Media files are rewritten only under the claims
   record's re-embed switch, off by default.

Only the operator, an Owner or an Administrator may read the claims, edit
the chapters or pin a claim, as with every other metadata edit. Anyone who
may see the edition's work may read the chapters it plays.

## Consequences

- An acquire records every list it computed (store markers or part names,
  the provider's layout and the aligned result), so a better source
  outranks a worse one without a new download.
- An Administrator's edit survives later acquires and provider answers
  until it is removed or another claim is pinned.
- The database gains four tables, written through SQL-v1 and tested under
  every database profile. Lists no claim names any more stay behind until
  a cleanup job removes them.
- Pins live on the claim rows rather than in `resolution_overrides`, and
  claims keep no history of earlier values yet.
- The claims record's planned `edition_metadata_changed` event is not
  added; `edition_updated` with a `fields` list carries the change.
