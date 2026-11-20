# ADR: Drop folders are a host role on storage instances

- **Status:** Accepted (decisions made with the maintainer on 2026-11-20)
- **Date:** 2026-11-20
- **Related:** [Storage destinations](storage-destinations.md),
  [Metadata claims and file sync](metadata-claims-and-file-sync.md),
  [Event delivery model](event-delivery-model.md),
  [Untrusted input fuzzing](untrusted-input-fuzzing.md)

## Context

Patrons want to copy books into a folder and have them show up in the
library, without the web app. Uploading a book in the web app is being
built at the same time, with one ingest job that matches, tags and stores
what it is given.

Storage plugins already list, read, move and delete files, and some report
outside changes (the local plugin's watcher, the S3 plugin's queue feed).
Every deployed storage instance is a destination
([Storage destinations](storage-destinations.md)), so a watched folder
must not also receive acquired books, or Bookclerk would ingest its own
writes.

## Decision

### 1. Intake is a host role on any storage instance

A storage instance is a destination unless its `core.intake` document, a
host document on the instance's scope, holds intake settings. Then it is a
drop folder. No new plugin, entrypoint or ABI field is added: any storage
plugin, first-party or not, can back a drop folder.

The role is read when the instance's guest starts, and a drop folder is
never handed to an acquire. So the role changes only while the instance is
deployed nowhere; the host refuses the change otherwise. A drop folder's
settings may change at any time.

### 2. The host groups and settles; the ingest job adds

The daemon on each node that runs a drop folder lists it, groups the
listing into items (each top-level file or folder), and records each
item's state in `intake_items`. An item whose files keep the same sizes and
signatures for `settle_seconds` is handed to the ingest job as one batch of
storage references into the folder, so nothing is copied to staging first.
The job checks the signature again before it reads.

Grouping and settling are sans-IO code in `bookclerk-intake`, fuzzed
against an independent oracle, because the listing is untrusted input.

### 3. Listing is the baseline; change feeds only make it sooner

A full listing every `scan_interval_seconds` works for every plugin,
including one that sleeps and a share that sends no events. A change feed
only starts a pass sooner. Claiming the work is the ingest job's, never an
event handler's ([Event delivery model](event-delivery-model.md)).

### 4. After ingest the item leaves the folder

By default an added item moves to `_imported/<date>/` and a failed one to
`_failed/` with a note saying why. `delete` and `leave` are settings; with
`leave` the stored signature keeps an item from being added twice. Moves
never replace an existing file.

### 5. Dropped books get a patron holding

Each dropped book is held by a patron: one named patron for the folder, or
one per top-level subfolder named after the patron's username. A book
whose files carry no strong identifier is still added and playable and
waits for confirmation; it is never matched by title alone.

## Consequences

- An operator turns a destination into a drop folder in three steps
  (undeploy, set, deploy), and the CLI and web API refuse the role change
  while it runs.
- A slow copy that pauses longer than `settle_seconds` can be picked up
  early; the job's signature check then fails it and it settles again, and
  operators can raise `settle_seconds` for slow shares.
- Drop folders and uploads share one ingest path, so matching, tagging and
  storage rules apply to both without special cases.
- `bookclerk-intake` and the settings reader are fuzzed boundaries listed
  in [`docs/contributing/fuzzing.md`](../contributing/fuzzing.md).
