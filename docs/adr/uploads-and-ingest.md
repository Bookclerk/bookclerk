# ADR: Uploads and ingest batches

- **Status:** Accepted; the ingest core is implemented (the upload API,
  staging, the `ingest_batch` job and `bookclerk library upload`). The web
  app's picker and the desktop window's native picker follow.
- **Date:** 2026-11-20
- **Related:** [Metadata claims and file sync](metadata-claims-and-file-sync.md)
  (amends its section 4), [Library data model](library-data-model.md),
  [Patron scopes](patron-scopes.md),
  [Untrusted-input fuzzing](untrusted-input-fuzzing.md)

## Context

Patrons want to add books they already have: a folder of MP3 files, an M4B,
or an archive someone sent them. A drop folder that a storage plugin
watches needs the same path from "some files" to "a book in the library".

Three things make this harder than copying files into storage:

- **The bytes are hostile.** File names can climb out of a folder, archives
  can expand without end, and tags are written by anyone.
- **The library needs an owner for every book.** The data model records who
  holds an edition (`holdings`). A store account holds what it bought; an
  uploaded book has no store account.
- **Uploads are large and connections break.** A 2 GiB audiobook over a
  phone connection must resume, and a person must not fill the server's
  disk.

## Decision

### 1. One batch per upload, one job per batch

A client declares everything it will send in one manifest
(`POST /api/uploads`): relative paths, sizes and optional SHA-256 digests.
The batch and its members are rows (`ingest_batches`, `ingest_members`).
Each file's bytes arrive in resumable `PATCH` requests that carry an
`Upload-Offset` header; the server keeps whatever arrived, so a broken
request resumes where the file stands. Committing queues one
`ingest_batch` job pinned to the node that holds the bytes.

A drop folder makes the same batch with `source = 'intake'`: each member
names an object in a storage instance instead of staged bytes, and the job
copies it in first. Everything after that is shared.

### 2. Uploaded books are held by a patron

An uploaded book gets a **patron holding**: `holdings.holder_kind =
'patron'` with `user_id`, entitlement `own`, over a listing of the
built-in `core.import` storefront whose product id is a digest of the
book's first audio file. This amends section 4 of
[Metadata claims and file sync](metadata-claims-and-file-sync.md), which
said an imported book has no holding.

A holding keeps every rule the library already has: the patron sees the
book in their scope, deleting the person deletes the holding, and the
resolver joins the listing to an existing edition only on strong evidence.
Acquire jobs never pick a patron holding, because only account holdings
reach a store.

The operator has no person of their own, so the operator's uploads are
held by the library's first active Owner.

### 3. Identification never trusts a title

The job groups files into books: everything in one folder is one book
(audio in natural order, then covers, ebooks and sidecars), and each loose
audio file at the top is its own book. It reads the first audio file's tags
in the confined tag worker and looks for an ASIN or ISBN in the tags or in a
bracketed token of a folder or file name (`The Hobbit [B0ABCDEF12]`).

Only those identifiers can join an existing edition. A book with neither is
still stored, under a new local edition, and its files are marked
`needs_review` for an Administrator, who can pin the listing to the right
edition later. A book whose edition is already acquired is reported as a
`duplicate` and not written again.

### 4. Archives are expanded in the media worker, under caps

ZIP (with Zip64 and Deflate64) and tar, plain or gzip, are expanded first.
7z may follow; RAR never will (no permissive decoder exists). The format is
sniffed from the bytes, never the name.

The `bookclerk-archive` crate decides what an archive may write: entry
names pass the same path policy as manifest paths, links and devices are
never created, nested archives are not expanded, and every cap counts bytes
written rather than sizes the archive declares (entries, total, per entry
and a compression ratio). The walk runs in the confined media worker, which
writes into an empty folder the host made. The host re-checks the worker's
report against the caps and the files on disk before it trusts it.

### 5. Limits are per node and fail closed

`[uploads]` sets the limits on each node: 20 GiB per file, 20 GiB staged
per person across their live uploads, a free-disk floor of 10% and pruning
after 24 hours idle. A request that would pass a limit is refused before
any byte is written. Who may upload at all is the `upload` member control,
off unless an Owner turns it on.

## Consequences

- Uploading is one more way to fill storage. The quota, the floor and the
  idle prune bound it on each node; the member control decides who.
- The manifest and the `Upload-Offset` header are an untrusted boundary
  with their own fuzz target (`upload_manifest`); archives have
  `archive_extract` and the worker's report `archive_report`.
- Duplicate detection compares editions, not quality: a better copy of a
  book the library already has is reported as a duplicate. Comparing
  quality is later work.
- Sidecar files (CUE, NFO, OPF) are stored with the book but not read for
  identification yet; the claims resolver will read them.
- `[uploads]` is bootstrap TOML per node for now. Moving it into the
  library database with the other settings is later work.

## Order of work

1. Ingest core: tables, patron holdings, the upload API, staging, the job,
   the CLI command and the fuzz targets.
2. The web app: a picker for files and folders, progress, resume and the
   review list.
3. The desktop window: the native file picker and drag and drop.
