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

An S3 ETag is an opaque identity hint. It is never treated as SHA-256, including multipart ETags, which are composite part tags. When a digest is supplied it is checked while the body is written. A caller-supplied digest that disagrees with the source probe is rejected before any byte is published. Size alone does not skip publication: `AlreadyPublished` requires the stored whole-object SHA-256 as well.

Local integrity lives in `{key}.bookclerk-meta.json` for that exact object. `book.m4b` and `book.jpg` do not share a stem sidecar. `probe` loads `sha256_hex` and `commit_token` from that record. Descriptive fields (ASIN, title, timestamps) stay descriptive; they are not integrity. A missing stage plus an existing destination counts as commit success only when that object's `commit_token` equals the retry-stable token and the stored digest is the one commit returns.

S3 user metadata is fixed at `CreateMultipartUpload`, before the body exists. After the upload hash is known, and before `CompleteMultipartUpload`, the backend writes a small `{key}.bookclerk-meta.json` object (one extra `PutObject` of a few hundred bytes, not a second copy of the audiobook). `probe` reads that sidecar when the HEAD metadata has no SHA-256. A later HEAD from a reconstructed client returns the digest. If the sidecar write fails, the multipart upload is aborted and publication is not successful.

## Ranges

`ObjectProbe.size` is the whole object, including for a ranged read. `ByteRange.length == None` reads through EOF. The Cap'n Proto field uses `0` for that case and decodes it to `None` before a backend sees it. `Some(0)` is rejected so local and S3 cannot disagree. Offsets past the object and `offset + length` overflow are errors. HEAD does not download the body.

## Pagination

Local `list_page` builds an on-disk external sort under `{root}/.bookclerk-list-index` when `cursor` is absent, in chunks of 2048 keys. Each build uses its own directory under `builds/` and publishes with a generation file so a slower build cannot replace a newer one, and so two prefixes do not share `run-0.bin`. A build that loses the race reads the published index. Readers that already opened the previous file keep that inode across the rename. A missing or deleted cursor key is `InvalidCursor` and does not restart at page one. Record lengths above 64 KiB are rejected instead of allocated. The index directory must be a real directory inside the storage root; a symlink is rejected. Abandoned build directories whose lock is not held are removed on the next build. Active builds are left alone. The index is node-local scratch, not a portable checkpoint. Concurrent creates/deletes during a scan are weakly consistent with the snapshot taken at the fresh scan. Insertion after the snapshot can be missed until the next `cursor == None` rebuild. The job layer may restart explicitly after `InvalidCursor`.

S3 cursors are the service continuation token. A repeated token or a truncated page without a token is `InvalidCursor`. Empty pages are followed at most 8 times.

Fan-out walks children in configuration order. The cursor is `bcf1` plus the child index and that child's cursor. Children are not sort-merged: opaque S3/plugin cursors are not lexicographic keys. Duplicate keys keep the earlier child (source precedence) and are skipped with `exists` on previous children. One page of objects is retained.

Internal keys (`.bookclerk-stage`, `.bookclerk-list-index`, `.bookclerk-tmp`) are not listed as completed media.

## Transfers and cancellation

`transfer_object` streams source to destination unless a same-instance server copy applies. Each attempt reopens the source. A partially consumed reader is not retried. The streamed path writes an attempt-owned key under `.bookclerk-stage/{attempt}/`, checks the source size, ETag, and digest, then publishes with `copy` when the destination supports it (otherwise a second bounded stream from the stage). Failure deletes that stage only. An existing destination, including one a concurrent writer replaced, is not deleted. A same-instance S3 copy pins every `CopyObject` and `UploadPartCopy` with the probed ETag (`x-amz-copy-source-if-match`). A changed source aborts the copy and does not complete a mixed object. The 5 GiB `CopyObject` ceiling is unchanged; tests may shrink the part size to exercise both sides without a multi-gigabyte body. That fixture is not live AWS conformance. Completed destinations whose stored digest and size already match are skipped. Interrupted objects restart from byte 0; a range read is not a resumable write. Publication is at-least-once relative to database leases, not exactly-once. Multi-destination sync is not a distributed transaction: a failed destination does not roll back one that already published.

Fan-out `put_stream` keeps the cancel guard armed until every child `put_stream` has been joined. Source EOF is not child completion. A drop during the join, or one child's finalization error, aborts and reaps the remaining children. Dropping a `JoinHandle` is not the cleanup path.

Local temp files are removed by a `Drop` guard. S3 multipart uploads are aborted on the error path and, if the future is dropped, from a spawned abort. The destination guest records upload ids under `$HOME/multipart-journal` (the jail-granted plugin data directory), not `$BOOKCLERK_FILES_DIR`. The record stores endpoint, bucket, prefix, key, upload id, and an owner id. The owner holds a lock for the life of the backend. A second backend aborts a record only when the endpoint, bucket, and prefix match and the owner lock is not held. A failed journal write aborts the upload instead of continuing as if recovery existed. In-process backends without a journal do not claim crash recovery. `NoSuchUpload` clears the record. Other abort failures increment a counter and stop automatic retries after 8 attempts; the record stays. Corrupt records are renamed aside. Credentials are not written to the journal.

## Scans and jobs

`storage_scan_rows` holds object and identity rows for one generation. The generation id is a random UUID (`scan-{uuid}`), not a PID. `storage_scan_generations` binds that id to the storage `instance_id` and, when the scan is fenced, the job id. A reused id cannot adopt another instance. The checkpoint (`schema_version` 1, op `storage_scan`) stores instance id, namespace, generation, phase (`list` or `apply`), and cursor. Rows for a page are inserted, then the cursor advances. A crash replays the page (inserts are idempotent) and does not skip it. A stale cursor deletes that generation and restarts once. Cancel and a lost fence stop the scan between pages. `run_acquire` scans once under the job fence and passes that index to storage matching, so a restart does not list the namespace again before the checkpoint. The generation is deleted when the job finishes successfully. Startup reclaim removes generations whose job is terminal or missing, and jobless generations whose heartbeat is older than the lease. A generation owned by a pending or running job is kept even if the heartbeat is old. Clearing Acquired status happens only after phase `apply`, and only when a fresh `exists` says the stored key is gone. A listing error does not clear. A checkpoint for a different `instance_id` fails closed.

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

The 256 MiB object is larger than the 32 MiB allowed delta and was fully published (`metadata.len` matched). Parent RSS did not track object size. That run is an in-process diagnostic. It does not impose a memory limit and it is not #120 acceptance for the host/workerd/guest tree. A missing `VmHWM` reading fails the test instead of being treated as zero.

External-path acceptance:

```text
cargo test -p bookclerk-workerd --test conformance external_native_behind_workerd_budget -- --ignored --nocapture --test-threads=1
```

It places the workerd gateway and the local destination guest in a child cgroup, streams an object larger than `memory.max`, checks the destination SHA-256, repeats a smaller object under the same budget, and lists 100,001 keys through that guest.

Measured on this Linux cloud agent (debug build, cgroup v2 `memory.max` = 402,653,184 bytes, `memory.peak`):

| Workload | Result |
| --- | --- |
| 32 MiB object through native-behind-workerd local | peak 105,603,072 bytes |
| 416 MiB object (larger than the budget), SHA-256 matched the zeros digest | peak 402,653,184 bytes, elapsed 21,320 ms for the whole test |
| List 100,001 keys through that guest | 391 pages, index directory created under the output root |

The 416 MiB object completed inside the cgroup. Peak for that run sat on the configured maximum; the 32 MiB run in the same cgroup stayed near 101 MiB, so the counter is not stuck at the cap. Live AWS multipart copy was not executed. The S3 tests are a local HTTP fixture (create, upload, copy-if-match, complete, abort), not service conformance. The 5 GiB `CopyObject` split remains a unit test of the selection function. Postgres was not running in this environment (`BOOKCLERK_TEST_POSTGRES_URL` unset); the new `storage_scan_generations` statement is in the unreleased migration pack and was applied on SQLite.
