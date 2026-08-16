# Durable job queue

`bookclerkd` admits background work as **durable rows** in `library.db` (`jobs`
+ `job_temp_paths`). HTTP and the interval scheduler are producers; leased
workers claim jobs. There is no external broker.

This is not a general pub/sub bus. Domain events such as `book.acquired` /
plugin `onEvent` stay on a separate notification path (today:
`notify_integrations`). They must not become job kinds. A transactional outbox
for those events is a later change.

See [architecture.md](architecture.md) and [operations.md](operations.md).

## Command envelope

Each row stores a versioned JSON command (`JobPayload.v`, currently `1`).
Unknown kinds, unsupported versions, and malformed JSON are rewritten to
`kind=invalid` / `state=failed` / `error_kind=invalid_job` and never run a
handler.

Dispatch goes through `JobTransport` (`InProcessJobTransport` today; a workerd
adapter can replace it later). Handlers receive a `JobExecCtx` with the lease
fence and a cooperative cancel flag.

## State machine

```text
pending → running → succeeded
                 ↘ failed (retry → pending with backoff, or terminal)
                 ↘ cancelled
```

- **Admission** is one atomic backend operation (`dbAtomic` in production,
  a local `BEGIN` in tests). A partial unique index on `dedup_key` for
  `pending`/`running` rows enforces active-key uniqueness.
- A **worker** claims with a conditional `pending` → `running` update and a
  unique per-attempt `lease_generation`. Lost RPCs retry the same
  `operation_id` so a replay cannot claim a second row. That replay is
  bounded (2s); if the database stays `Unavailable`, the worker returns the
  error, drops the `job_runtime` read permit so a plugin swap can proceed,
  and lets an ambiguously claimed row expire for reclaim. The next loop
  iteration uses a new operation id.
- Heartbeat, progress, complete, fail, and reclaim are fenced on
  `(job_id, lease_owner, lease_generation)`. Zero rows (`Ok(false)`) means the
  fence is lost; the worker cancels the handler and ignores the unfenced
  result. A heartbeat **error** (transient DB/RPC failure) is retried only
  until the last confirmed lease expiry; past that deadline the worker
  cancels locally and ignores the result. It must not finalize the row as
  `cancelled` unless `cancel_requested` is set.
- Destination **publish** is not atomic with that fence. `plugin_copy`
  stream-copy is at-least-once: a lost lease can still make object bytes
  visible after the last heartbeat. Duplicates are absorbed by retry-stable
  `commit_token`s (idempotent local/S3 `commit` when the dest already exists).
- Reclaim also requires `lease_expires_at` to still be null or `<= now`, so a
  heartbeat that extends the same generation cannot be stolen.
- `POST /api/jobs/{id}/cancel` cancels `pending` immediately and flags
  `running` for a cooperative stop between acquire titles, scan sources, and
  listen-sync providers. Cancel retries if a worker claims the row between the
  pending read and the pending-only CAS, so a cancel that returns never leaves
  a running job unflagged. Integration `scan_library` RPCs are not interruptible;
  cancel is checked before the call and a repeat remote scan is idempotent.

## Job kinds

| Kind | Dedup key | Resource class | Handler |
| --- | --- | --- | --- |
| `scan` | `scan:account={id\|all}` | `network` | `run_scan` |
| `acquire` | `acquire:title={id\|all}:account={id\|all}` | `network` | `run_acquire` |
| `listen_sync` | `listen_sync` | `network` | `run_listen_sync` |
| `integration_scan` | `integration_scan:id={id}:force={0\|1}` | `network` | `run_integration_scan` |
| `plugin_copy` | `plugin_copy:plugin={id}:from={key}:to={key}` | `network` | ABI v2 `JobHandler` stream-copy (`run_plugin_copy`) |

Reserved classes (no worker in this release): `media`, `transcription`,
`indexing`.

## Admission and backpressure

`POST /api/library/scan`, `/acquire`, `/api/discover/sync-listening`, and
`/integrations/{id}/scan` (and the scheduler) call
`LibraryStore::enqueue_job`:

| Outcome | HTTP | Meaning |
| --- | --- | --- |
| Created | 200 | New row; worker is notified |
| Duplicate | 409 | Same dedup key already `pending` or `running`; `job_id` is the existing row |
| QueueFull | 429 | `pending + running >= jobs.max_pending` |

Admission, claim, and handler execution share an `RwLock` (`job_runtime`).
A database plugin swap takes the write permit so it cannot observe an idle
worker that is about to claim. Admission waits up to 15s for a read permit
and returns `503` if a swap holds the lock too long.

PostgreSQL admission and scratch-quota updates take
`pg_advisory_xact_lock(88118)` (SQLite/D1 lock the `job_queue_control`
singleton) so `COUNT` then `INSERT` cannot exceed `max_pending` under
`READ COMMITTED`. The required `postgres job queue` CI job runs those
concurrency tests against a disposable multi-connection database
(`BOOKCLERK_TEST_POSTGRES_URL` + `BOOKCLERK_REQUIRE_POSTGRES_TESTS=1`).
They are `#[ignore]` in the default workspace suite so a missing Postgres
cannot false-pass. TOTP enroll/disable `dbAtomic` conformance
(`postgres_totp_*`) is not ignored: the same job provisions Postgres and
runs those tests automatically (`BOOKCLERK_REQUIRE_POSTGRES_TESTS=1`).

Claim (native and D1) marks pending rows with malformed JSON, an unknown kind,
an unknown `resource_class`, or an unsupported envelope version as
`invalid_job` before class-specific selection, so a bad row cannot occupy
`max_pending` forever or abort a D1 batch.

CLI `bookclerk library scan/acquire` still runs in-process and does not use
this queue.

## Leases and crash recovery

- Default lease is 60s (`jobs.lease_seconds`); the worker heartbeats at
  `lease/3`. `Ok(false)` (fence lost) sets the handler cancel flag. A
  heartbeat `Err` is logged and retried only until the last confirmed
  `Ok(true)` (or the original claim) would have expired; after that the
  worker treats the result as unfenced and ignores it. Finalization calls
  `fail_job(..., "cancelled")` only when `cancel_requested` is set. A local
  cancel without that flag is treated as fence loss and ignored.
- Startup and each worker tick call `reclaim_expired_leases`.
- Books left `queued` / `downloading` with **no** running acquire job are set
  to `error` (`orphaned_after_restart`). The next acquire job retries them.
- Scratch dirs under `{cache}/acquire` and `{cache}/acquire-pdf` are reserved
  against `jobs.temp_quota_bytes` (default 2 GiB) and unregistered only after
  that path is deleted (or already gone). Startup sweeps unregistered orphans.
  Fetch, remux/transcode, and companion-PDF downloads (PDF-only and sidecars
  discovered during audio acquire) are bounded by the remaining quota: sources
  can call `FetchOptions::enforce_cache_budget`, the pipeline watchdog cancels
  a stage that crosses the budget, and PDF bodies are streamed with an
  explicit remaining-byte cap.

Scan, listen-sync, and integration-scan retries are idempotent (upserts /
remote rescan). Acquire retries skip titles already `acquired`, except when
`download_pdf` is on and the companion PDF is not stored (`pdf_status` is
not `acquired`, or `pdf_storage_key` is missing/empty). Those rows stay in
the job target list so a later acquire can resume the PDF-only side effect.
A companion PDF that exceeds the remaining scratch budget is soft-failed
(`pdf_status=error`) so the audio acquire can succeed. Dedicated
`acquire --pdf` still fails the job when the PDF cannot be stored; that
path always unregisters its `acquire-pdf` scratch reservation, including
on fetch/quota errors.

## Resource-class concurrency

`[jobs.concurrency].network` is the number of leased network workers (default
**1**). The historical `work_lock` still wraps `run_*` so store rate limits
and the shared `StorageIndex` stay single-writer. Codec work stays in the
`[media].workers` pool.

## Adding a job kind

1. Add a `JobKind` variant, envelope fields, and dedup-key rule in
   [`crates/bookclerk-library/src/models.rs`](../crates/bookclerk-library/src/models.rs).
2. Add a `JobCommand` arm on [`InProcessJobTransport`](../crates/bookclerkd/src/job_handler.rs).
3. Enqueue from the API and/or scheduler with the same `EnqueueJobSpec`.
4. Reserve scratch paths via `LibraryStore::reserve_job_temp_path` when the
   job creates temp files; unregister one path after it is gone.
5. Add concurrent admission/claim, fence-loss, and restart tests.

## Configuration

See [configuration.md](configuration.md) (`[jobs]` and `BOOKCLERK_JOBS_*`).
