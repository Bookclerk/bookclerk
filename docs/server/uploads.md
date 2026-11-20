# Uploads

A patron can add books they already have by uploading files, whole
folders, or ZIP and tar archives. Bookclerk works out which files make
each book, finds the edition when the files name one, and stores the book
in the library's destinations under the usual naming. The patron who
uploaded a book holds it, just as a store account holds what it bought.

This page covers who may upload, the limits, what happens to the files,
and the HTTP routes. The design is in the
[uploads ADR](../adr/uploads-and-ingest.md).

## Who may upload

Uploading is the **Upload** member control. It is off for Members until an
Owner or Administrator turns it on, for everyone (the library default) or
for one person; see
[Limit what a patron may do](../guide/sharing.md#limit-what-a-patron-may-do).
Owners and Administrators can always upload.

Uploads made with the operator token, such as `bookclerk library upload`,
are held by the library's first active Owner.

## Limits

Each node enforces its own `[uploads]` limits (see
[Upload limits](configuration.md#upload-limits)):

| Limit | Default | When it is checked |
| --- | --- | --- |
| Largest file | 20 GiB | When the upload starts |
| Bytes one person may have waiting in unfinished uploads | 20 GiB | When the upload starts |
| Free disk the node keeps | 10% | When the upload starts and before each file's bytes |
| Idle time before an unfinished upload is cancelled | 24 hours | Every 15 minutes |

An upload names at most 10,000 files. Paths are relative and keep their
folders. A path that is absolute, climbs out with `..`, or folds onto
another path (by case or Unicode form) is refused.

## What happens to the files

1. The client sends a manifest of every file, then each file's bytes.
   A broken transfer resumes where the file stands.
2. The patron commits the upload. Bookclerk queues an `ingest_batch` job
   on the node that holds the bytes.
3. The job checks each file's size and, when the manifest gave one, its
   SHA-256.
4. Archives are expanded in the confined
   [media worker](media.md#uploaded-archives). Each archive's files go
   under a folder named after the archive.
5. Files are grouped into books: everything in one folder is one book,
   and each audio file at the top level is a book of its own. Covers,
   EPUB and PDF files and text sidecars stay with their book. Files of
   other types are refused.
6. Each book is identified from its first audio file's tags and from an
   ASIN or ISBN in square brackets in its folder or file name, such as
   `The Hobbit [9780261102217]`. A title alone never joins an existing
   edition.
7. The book's files are written to every destination, named by the
   destination's templates with the edition's `<call number>`.

Every file ends with one outcome:

| Outcome | Meaning |
| --- | --- |
| `matched` | Stored under the edition an ASIN, an ISBN or an earlier upload named |
| `needs_review` | Stored under a new local edition, because nothing identified it; an Administrator should check it |
| `duplicate` | The library already has this edition; nothing was written |
| `expanded` | An archive whose files were added to the upload |
| `refused` | Not stored, with a reason such as `unsupported_type`, `digest_mismatch`, `no_audio`, `nested_archive` or `link` |
| `failed` | Could not be stored, with a reason such as `disk_full`, `storage_write_failed` or `media_worker_unavailable` |

When the job ends, the host publishes the `ingest_batch_finished` event
with the upload's id and final state, so a plugin can follow it (see
[Plugin ABI](../plugins/abi.md)).

> [!NOTE]
> ZIP (including Zip64 and Deflate64 entries) and tar, plain or gzip, are
> expanded today. 7z may follow. RAR archives are refused.

## Review uploaded books

`GET /api/uploads/review` lists the books stored as `needs_review`, oldest
upload first. When one is a known edition, pin its listing to that edition
(`PUT /api/library/listings/{id}/pin`) and the book moves there with its
files.

## From the command line

```sh
bookclerk library upload "~/Audiobooks/The Hobbit" dune.zip
```

The command needs a running `bookclerkd`. It prints each file's outcome
when the job ends; `--no-wait` returns once the upload is committed. See
the [CLI reference](cli-reference.md).

## HTTP routes

Every route needs a signed-in patron or the operator token.

| Method | Path | What it does |
| --- | --- | --- |
| `POST` | `/api/uploads` | Starts an upload from a manifest: `{"files": [{"path", "size", "sha256"?}]}` |
| `GET` | `/api/uploads` | The caller's 50 newest uploads |
| `GET` | `/api/uploads/{id}` | One upload with every file's `received` bytes and outcome |
| `PATCH` | `/api/uploads/{id}/files/{member_no}` | Sends bytes of one file from `Upload-Offset`, as `application/offset+octet-stream` |
| `POST` | `/api/uploads/{id}/commit` | Queues the ingest job once every file is complete |
| `DELETE` | `/api/uploads/{id}` | Cancels an upload that has not started ingesting |
| `GET` | `/api/uploads/review` | Books to review (an Administrator) |

A `PATCH` whose `Upload-Offset` is not where the file stands is refused
with `409` and the file's current length; read the upload and resume from
`received`. The [Web API](api/README.md) reference has every request and
response.

## Next steps

- [Media worker](media.md): the confined process that expands archives and
  reads tags.
- [Output and destinations reference](output.md): how files are named in
  each destination.
