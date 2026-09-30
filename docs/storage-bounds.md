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

S3 user metadata is fixed at `CreateMultipartUpload`, before the body exists, so a digest computed while streaming cannot ride in that metadata. After `CompleteMultipartUpload` returns an ETag, the backend writes `{key}.bookclerk-integrity/{hex(etag)}.json` (a few hundred bytes, not a second copy of the audiobook). `probe` reads that object only when its stored ETag equals the current HEAD ETag, and stops after 64 KiB even when `Content-Length` is missing or understated. A larger hint is rejected before the body is read. A record for any other ETag is ignored, including one left by a failed or interleaved replacement. A missing record means the digest is unknown, not that another version's digest applies. `CopyObject` copies inline user metadata only, so a small stage-to-final copy writes a new record for the ETag returned by that `CopyObject`, not for a later HEAD. The digest survives commit replay, and a replacement that lands before a follow-up HEAD does not inherit it. `delete` removes the object, every record under that key's integrity prefix, and a legacy `{key}.bookclerk-meta.json` if one is still present, so a later object at the same key cannot inherit it. An integrity write that fails after the body is complete leaves the new bytes without a digest; it does not keep the previous version's digest. Multipart ETags are not SHA-256.

## Ranges

`ObjectProbe.size` is the whole object, including for a ranged read. `ByteRange.length == None` reads through EOF. The Cap'n Proto field uses `0` for that case and decodes it to `None` before a backend sees it. `Some(0)` is rejected so local and S3 cannot disagree. Offsets past the object and `offset + length` overflow are errors. HEAD does not download the body.

## Pagination

Local `list_page` builds an on-disk external sort under `{root}/.bookclerk-list-index` when `cursor` is absent, in chunks of 2048 keys. Each build is created under `.preparing/<uuid>/` and moved into `builds/<uuid>/` only after `active.lock` is held, so a sweeper that only visits `builds/` cannot delete or lock a build that is not yet owned. The lock stays on that inode across the rename. A generation file keeps a slower build from replacing a newer one, and two prefixes do not share `run-0.bin`. A build that loses the race reads the published index. Readers that already opened the previous file keep that inode across the rename. A missing or deleted cursor key is `InvalidCursor` and does not restart at page one. Record lengths above 64 KiB are rejected instead of allocated. The index directory must be a real directory inside the storage root; a symlink is rejected. Abandoned `builds/` directories whose lock is not held are removed on the next build. The sweeper holds that lock only long enough to decide the directory is abandoned, then removes it. Active builds are left alone. The index is node-local scratch, not a portable checkpoint. Concurrent creates/deletes during a scan are weakly consistent with the snapshot taken at the fresh scan. Insertion after the snapshot can be missed until the next `cursor == None` rebuild. The job layer may restart explicitly after `InvalidCursor`.

S3 cursors are the service continuation token. A repeated token or a truncated page without a token is `InvalidCursor`. Empty pages are followed at most 8 times.

Fan-out walks children in configuration order. Its `instance_id` is a length-prefixed encoding of those child ids in that order, not a sorted join. The cursor is `bcf2` plus a hash of that identity, the child index, and that child's cursor. A cursor from a different order is `InvalidCursor`. Children are not sort-merged: opaque S3/plugin cursors are not lexicographic keys. Duplicate keys keep the earlier child (source precedence) and are skipped with `exists` on previous children. That is one HEAD per key per earlier child. It is not an integrity hole. Replacing it needs a bounded on-disk seen-set with its own owner, cursor lifetime, and cleanup; an in-memory set of every key is not acceptable, and plugin cursors are not a global sort. One page of objects is retained. A follow-up tracks a bounded dedup design (#233).

Internal keys (`.bookclerk-stage`, `.bookclerk-list-index`, `.bookclerk-tmp`) are not listed as completed media.

## Transfers and cancellation

`transfer_object` streams source to destination unless a same-instance server copy applies. Each attempt reopens the source. A partially consumed reader is not retried. The streamed path writes an attempt-owned key under `.bookclerk-stage/{uuid}/`, checks the source size, ETag, and digest, then publishes with `copy` when the destination supports it (otherwise a second bounded stream from the stage). The stage is armed in a lease before `put_stream`. Explicit failure, drop, attempt timeout, and caller cancellation all delete that stage. Same-instance copy probes the source first and rejects anything above `MAX_SUPPORTED_OBJECT_BYTES` without writing the destination. Direct local `copy` applies the same cap before `fs::copy`. When `stage_journal_dir` is set (acquire sync uses `$BOOKCLERK_FILES_DIR/stage-journal`), record creation must succeed before `put_stream`. The JSON is written and synced through the writable handle of a `.json.partial` file, that handle is closed, then the file is renamed into the scanned name. The owner lock is removed while it is still held, after the record is gone. Opening that lock and acquiring it share `journal.lock` with the reaper, so a sweep cannot unlink the file in the gap before `flock`. `journal.lock` is not held across remote delete or upload. Recovery and debris cleanup walk filenames in order and retain at most 8 records or owner ids at a time, so a failed delete does not block later records and the pass does not keep the whole inventory. A failed remote delete keeps the record and the lock. The reaper removes abandoned partials and owner files that no remaining record names, and it will not unlink a lock another live attempt holds. A read-only reopen is not used: Windows `FlushFileBuffers` requires write access. The reaper ignores partial files. A failed delete leaves the record. The next transfer reaps records whose owner lock is not held and whose storage instance matches, and skips a live attempt. An existing destination, including one a concurrent writer replaced, is not deleted. An existing destination, including one a concurrent writer replaced, is not deleted. A same-instance S3 copy pins every `CopyObject` and `UploadPartCopy` with the probed ETag (`x-amz-copy-source-if-match`). A changed source aborts the copy and does not complete a mixed object. The 5 GiB `CopyObject` ceiling is unchanged; tests may shrink the part size to exercise both sides without a multi-gigabyte body. That fixture is not live AWS conformance. Completed destinations whose stored digest and size already match are skipped. Interrupted objects restart from byte 0; a range read is not a resumable write. Publication is at-least-once relative to database leases, not exactly-once. Multi-destination sync is not a distributed transaction: a failed destination does not roll back one that already published.

Fan-out `put_stream` keeps the cancel guard armed until every child `put_stream` has been joined. Source EOF is not child completion. A drop during the join, or one child's finalization error, aborts and reaps the remaining children. Dropping a `JoinHandle` is not the cleanup path.

Local temp files are removed by a `Drop` guard. S3 multipart uploads are aborted on the error path and, if the future is dropped, from a spawned abort. The destination guest records upload ids under `$HOME/multipart-journal` (the jail-granted plugin data directory), not `$BOOKCLERK_FILES_DIR`. The record stores endpoint, bucket, prefix, key, upload id, and an owner id. The owner holds a lock for the life of the backend. A second backend aborts a record only when the endpoint, bucket, and prefix match and the owner lock is not held. A failed journal write aborts the upload instead of continuing as if recovery existed. In-process backends without a journal do not claim crash recovery. `NoSuchUpload` clears the record. Other abort failures increment a counter and stop automatic retries after 8 attempts; the record stays. Corrupt records are renamed aside. Credentials are not written to the journal.

## Scans and jobs

`storage_scan_rows` holds object and identity rows for one generation. The generation id is a random UUID (`scan-{uuid}`), not a PID. `storage_scan_generations` binds that id to the storage `instance_id` and, when the scan is fenced, the job id. Local filesystem backends, fan-out that includes one, and plugin storage wrappers also record host placement (`event_node_id` when `BOOKCLERK_FILES_DIR` is set). A checkpoint is adopted only when that placement matches. Object stores leave placement unset and stay portable. Inventory rows and the generation row are removed in one transaction. A confirmed terminal job drops its inventories while the daemon is up; a pending retry does not. A reused id cannot adopt another instance. The checkpoint (`schema_version` 1, op `storage_scan`) stores instance id, namespace, generation, phase (`list` or `apply`), and cursor. Rows for a page are inserted, then the cursor advances. A crash replays the page (inserts are idempotent) and does not skip it. A stale cursor deletes that generation and restarts once. Cancel and a lost fence stop the scan between pages. `run_acquire` scans once under the job fence and passes that index to storage matching. Adopting a checkpoint requires the generation row to still exist for that instance. An `apply` checkpoint is adopted only when that row is marked complete; a missing or incomplete generation starts a new scan instead of returning an empty index. A `list` checkpoint resumes only when its row exists. The generation is deleted on success, including when the target set is empty, before the job handler returns. A crash after that delete and before the job is terminal therefore rescans. Startup reclaim removes generations whose job is terminal or missing, and jobless generations whose heartbeat is older than the lease. A generation owned by a pending or running job is kept even if the heartbeat is old. Clearing Acquired status happens only after phase `apply`, and only when a fresh `exists` says the stored key is gone. A listing error does not clear. A checkpoint for a different `instance_id` fails closed.

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

Partial external measurement (not acceptance). The workerd conformance test moves only the gateway and guest into the cgroup and starts the guest with `Command`, not `bookclerk-jail`. Its `memory.peak` includes reclaimable file cache. It stays labeled partial.

```text
cargo test -p bookclerk-workerd --test conformance external_native_behind_workerd_budget -- --ignored --nocapture --test-threads=1
```

Previous partial numbers on this class of Linux agent (debug build, `memory.max` = 402,653,184): 32 MiB peak 105,603,072; 416 MiB object peak 402,653,184; 100,001 keys in 391 pages. That peak sat on the cap and is not evidence that anonymous memory tracked the object.

Production-path acceptance (host re-exec'd into the cgroup before its runtime, then `PluginSession` with required isolation). Passed on this Linux agent:

```text
BOOKCLERK_RESOURCE_ARTIFACT_DIR=/opt/cursor/artifacts cargo test -p bookclerk-plugin-host --test external_budget -- --ignored --nocapture --test-threads=1
```

| | |
| --- | --- |
| Processes | host in `…/bookclerk-budget-*/bookclerk-host`; gateway and guest in `…/bookclerk-session-…` under that same budget. Guest `Seccomp` = 2 |
| `memory.max` after startup | 285,949,952 bytes |
| 8 MiB object | sampled anon 33,480,704 |
| 353,058,816 byte object (larger than `memory.max`) | sampled anon 34,660,352; sampled file cache 244,822,016; on-disk SHA-256 matched a digest computed from the source bytes |
| 100,001 keys, exact ordered list | sampled anon 37,847,040; sampled file cache 253,419,520 |
| Lifetime `memory.peak` | 566,173,696. This is from cgroup creation, including startup under a 2 GiB ceiling, and is not the transfer bound |

Anonymous RSS stayed near 35 MiB while the object was 353 MiB. The cgroup total during the transfer was mostly file cache. `s3_journal_restart` killed a jailed `destination-s3` guest (`Seccomp` 2) after a multipart part was accepted and the restarted guest aborted that upload (6.6 s). That fixture is local HTTP, not live AWS. SQLite ran the scan-adoption tests. Postgres was not available here (`BOOKCLERK_TEST_POSTGRES_URL` unset); `storage_scan_generations.completed` is in the unreleased migration the existing PostgreSQL jobs apply.
