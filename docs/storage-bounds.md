# Bounded storage

Audiobook bytes and storage inventories must not determine process memory.
Scalar get/put are for objects at or below the ABI scalar cap (256 KiB,
`MAX_SCALAR_BYTES`, or a lower negotiated limit). Larger bodies use
`get_stream` / `put_stream`, a same-namespace server copy, or an explicitly
quota-managed scratch file.

This is the contract for #120. It does not change playback, and it does not
make destination publication exactly-once. See [jobs.md](jobs.md).

## Limits

| Boundary | Cap | Notes |
| --- | --- | --- |
| Scalar get/put | 256 KiB (`MAX_SCALAR_BYTES`) | HEAD/content-length is an early hint. The body is counted. `limit+1` is `PayloadTooLarge`, not a short success. The advertised size is not allocated. |
| Stream window | 1 MiB | ABI `ByteSource` pull. Unchanged. |
| List page | 256 objects | `limit` is clamped. Oversized pages are errors. |
| Checkpoint JSON | 64 KiB | Cursor and identity only. Per-object rows live in `storage_scan_rows`. |
| Application object size | 80 GiB | `8 MiB * 10,000` S3 parts. Above this, copy and upload fail before filling another part buffer. AWS allows larger objects; this process does not. |
| Single `CopyObject` | 5 GiB inclusive | Larger copies use `UploadPartCopy`. This matches AWS's documented `CopyObject` ceiling. It was not re-verified against live AWS in this change. |

`list` and `list_audio` are compatibility collectors. They return at most one page and error when the namespace is larger. Product scans call `list_page`.

## Identity and copy

`supports_server_copy` defaults to false. Local and S3 opt in. A copy runs only when both ends have the same `instance_id` (canonical root + prefix, or endpoint + region + bucket + prefix, or plugin session). Equal backend names are not identity. Two buckets, prefixes, or roots stream.

`copy` stays inside one backend. Staged publication (`commit`) uses that copy and is size-aware, so a multipart upload is not followed by a single `CopyObject` above 5 GiB.

An S3 ETag is an opaque identity hint. It is never treated as SHA-256. When a digest is supplied it is checked while the body is written, before the temp file is renamed or the multipart upload is completed. The computed digest is stored as user metadata / the local sidecar. A missing stage plus an existing destination counts as commit success only when that object's `commit-token` metadata equals the retry-stable token.

## Ranges

`ObjectProbe.size` is the whole object, including for a ranged read. `ByteRange.length == None` reads through EOF. The Cap'n Proto field uses `0` for that case and decodes it to `None` before a backend sees it. `Some(0)` is rejected so local and S3 cannot disagree. Offsets past the object and `offset + length` overflow are errors. HEAD does not download the body.

## Pagination

Local `list_page` builds an on-disk external sort under `{root}/.bookclerk-list-index` when `cursor` is absent, in chunks of 2048 keys. Later pages binary-search that file. A missing or deleted cursor key is `InvalidCursor` and does not restart at page one. The index is node-local scratch, not a portable checkpoint. Concurrent creates/deletes during a scan are weakly consistent with the snapshot taken at the fresh scan. Insertion after the snapshot can be missed until the next `cursor == None` rebuild. The job layer may restart explicitly after `InvalidCursor`.

S3 cursors are the service continuation token. A repeated token or a truncated page without a token is `InvalidCursor`. Empty pages are followed at most 8 times.

Fan-out walks children in configuration order. The cursor is `bcf1` plus the child index and that child's cursor. Children are not sort-merged: opaque S3/plugin cursors are not lexicographic keys. Duplicate keys keep the earlier child (source precedence) and are skipped with `exists` on previous children. One page of objects is retained.

Internal keys (`.bookclerk-stage`, `.bookclerk-list-index`, `.bookclerk-tmp`) are not listed as completed media.

## Transfers and cancellation

`transfer_object` streams source to destination unless a same-instance server copy applies. Each attempt reopens the source. A partially consumed reader is not retried. If size or ETag changes between the pre-read HEAD, the open, and the post-write HEAD, the attempt fails and a published dest from that attempt is deleted. Completed destinations whose stored digest and size already match are skipped. Interrupted objects restart from byte 0; a range read is not a resumable write. Multi-destination sync is not a distributed transaction: a failed destination does not roll back one that already published. Replay overwrites or skips via the digest check.

Fan-out `put_stream` owns one task per child and aborts those tasks when the parent is dropped or a child fails (`JoinHandle::abort`). Dropping a `JoinHandle` is not the cleanup path.

Local temp files are removed by a `Drop` guard. S3 multipart uploads are aborted on the error path and, if the future is dropped, from a spawned abort. Upload ids are recorded under `$BOOKCLERK_FILES_DIR/storage-orphans` when that directory can be created. A failed abort leaves the record and logs the upload id. `S3Backend::from_config` retries those aborts. `NoSuchUpload` clears the record. Records are not deleted before a successful abort.

## Scans and jobs

`storage_scan_rows` holds one page of object and identity rows at a time in the library database. The checkpoint (`schema_version` 1, op `storage_scan`) stores instance id, namespace, generation, phase (`list` or `apply`), and cursor. Rows are inserted, then the cursor advances. A crash replays the page (inserts are idempotent) and does not skip it. Clearing Acquired status happens only after phase `apply`, and only when a fresh `exists` says the stored key is gone. A listing error does not clear. A checkpoint for a different `instance_id` fails closed.

`checkpoint_running_job` writes that JSON under the lease fence without suspending the row. Reclaim leaves the payload in place.

Destination publish remains at-least-once relative to the database lease. A lost fence can still make bytes visible. Retry-stable commit tokens and digest checks make replay idempotent. This is not exactly-once publication.

## What was measured

Ignored tests (not the default `cargo test` suite):

```text
cargo test -p bookclerk-storage --lib list_page_one_hundred_thousand_objects -- --ignored --nocapture
cargo test -p bookclerk-storage --test resource_bounds -- --ignored --nocapture --test-threads=1
```

Measured on this Linux cloud agent (debug build, `/proc/self/status` `VmHWM`; cgroup memory files were not mounted):

| Workload | Result |
| --- | --- |
| Transfer 32 MiB sparse object | 759 ms, VmHWM 5,885,952 bytes |
| Transfer 256 MiB sparse object | 4,890 ms, VmHWM 5,951,488 bytes |
| RSS delta | 65,536 bytes (cap 1 GiB, allowed delta 32 MiB) |
| Local list of 100,001 objects | create 1,232 ms; 391 pages in 621 ms; 1 index build; sort chunk 2,048 |
| Fan-out of that tree (two views, duplicate keys collapsed) | 3,227 ms, 100,001 unique keys |

The 256 MiB object is larger than the 32 MiB allowed delta and was fully published (`metadata.len` matched). Parent RSS did not track object size. Host, workerd, and guest were not separate processes in that transfer; it ran in-process through `transfer_object` and `LocalFsBackend`. Range/HEAD through direct native and native-behind-workerd are covered by `bookclerk-workerd` conformance (`destination_ranges`). Live AWS multipart copy was not executed; the 5 GiB `CopyObject` split is a unit test of the selection function. An S3 emulator was not used.
