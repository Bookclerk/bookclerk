# ADR: Event delivery follows the Workers model

- **Status:** Accepted (decisions 1 to 4); later phases proposed
- **Date:** 2026-11-11

## Context

Plugins should be able to sleep while idle, as Cloudflare Workers do, and
the system should stay event-driven. That only works if the host decides
which plugins an event is for, and if a plugin that isn't running still
receives its events.

The host already matched events to declared `[[events.consumers]]` rows,
so nothing was broadcast to every plugin. Two things blurred the model:

- **Subscription followed process liveness.** The subscriber catalog
  marked a plugin enabled only while a node had its guest loaded. An
  instance that was deployed but not running got no deliveries at publish
  time and relied on the late-join reconcile to catch up.
- **One word for two ideas.** Docs and code said a node *claims* both
  jobs and event deliveries. A job is a command with one handler. An
  event is a fact that every subscriber receives. Using the same verb
  suggested subscribers compete for an event, which they never do.

## Decision

Bookclerk follows the Cloudflare Workers model, mapping each trigger to
one Workers primitive:

| Bookclerk | Workers model | Delivery rule |
| --- | --- | --- |
| `[[events.consumers]]` | Events fanned out to one queue per subscriber, drained by that Worker's `queue()` handler | Every subscribed instance gets its own copy |
| `[[queues.consumers]]` | A Queues consumer | Each message goes to the queue's one consumer |
| `job(job)` | A Workflow step, or a command sent to one Worker | One command, one handler |
| `[triggers] crons` | Cron Triggers | One fire per instance, cluster-wide |
| Storefront, storage, metadata and database entrypoints | Service binding RPC to a `WorkerEntrypoint` | The caller awaits a reply |

Everything is asynchronous. No host thread blocks on a plugin; RPC
callers await a promise, and long work is a durable job.

1. **Subscriptions belong to the deployment.** Each node heartbeats a
   catalog row for every instance its deployments run (`present` or
   `placed`), with the consented subscriptions of what it would run (the
   active installed version and its grant, or a host-local tree), whether
   or not the guest is up. A matching event gets a delivery for that
   instance at publish time, and the delivery waits as `pending` until
   the instance runs. Fan-out stays per instance.
2. **Deliveries are leased, jobs are claimed.** A lease on a delivery
   only picks the node that runs one subscriber's copy. The host API is
   `lease_next_event_delivery`. The persisted index
   `idx_event_deliveries_claim` and the named atomic operations
   `claimEventDelivery` / `claimNextEventDelivery`, which database
   plugins implement, keep their names, since renaming them is a contract
   change with no behavior gain.
3. **Deliveries have a retention period.** As with a Queues message, a
   delivery that was never handed to its subscriber expires (state
   `expired`) once it has been due for longer than the consumer's
   `message_retention_period`: 60 seconds to 14 days, one day by default,
   and never more than `[events].retention_days`. Leased, retried and
   suspended deliveries keep following `max_retries` and dead letters.
   Host and node consumers never expire, and a subscriber with no live
   catalog row waits the whole events window. Expiry is a best-effort
   hourly sweep that bounds the backlog, not a delivery deadline. Expired
   rows are kept for the events retention window as the record of what a
   subscriber missed.

4. **Plugins read the event stream to catch up.** A plugin pages past
   events from the outbox with `env.EVENTS.read({ after | since, types,
   limit })` (`EventPublisher.read`), keeping an opaque `(created_at, id)`
   cursor itself. A read returns only events of the caller's own library
   that the instance's consented subscriptions match by type, schema
   version and filter, within the invocation's account scope, and never
   older than `[events].retention_days` (`truncated` says the start was
   older). Each page names the events whose delivery to this plugin
   expired (`expiredEventIds`). A read is not a delivery: it changes no
   delivery state, `deliveryAttempt` is zero and the deduplication key is
   the event id. The `EVENTS` binding therefore exists for a consented
   consumer as well as a producer. Each read scans a bounded number of
   outbox rows and may return a short page with `more` set. A read stops
   30 seconds short of now, because `created_at` is stamped before the
   insert commits and a cursor must never pass a row that has yet to
   appear; the outbox refuses caller-chosen event ids that a cursor cannot
   name. The guest request and its Cap'n Proto codecs are a fuzzed
   boundary (`event_read`).

The following phases are proposed and get their own pull requests and
updates to this record:

- **On-demand activation.** A due delivery, queue message, job or cron
  fire, or an RPC call, starts an asleep instance; an idle timeout per
  family stops it with `PluginWorker.shutdown`. Cold-start time is
  measured per family first, and the RPC budget is set from those
  numbers. Placement keeps reserving budget for asleep instances.
- **Batched delivery.** `event(batch)` and `queue(batch)` receive up to
  `max_batch_size` messages per call.
- **Richer host-side filters.** Envelope selectors and per-instance
  narrowing that can only intersect the consented subscription.

## Consequences

- An instance that is deployed but failing to start holds `pending`
  deliveries for the events it subscribes to only for its retention
  period. Deliveries that are still due when it runs drain then; older
  ones expire, and the plugin catches up from the event stream with
  `env.EVENTS.read`, or resyncs from its source of truth when the gap is
  older than the events window. An operator who gives up on it undeploys it (its
  row expires within the 60 s catalog TTL).
- A consumer that is running but falls behind by more than its retention
  period also loses the oldest never-leased deliveries. Consumers that
  expect long backlogs declare a longer `message_retention_period`.
- Events are scoped to one library, the cluster that shares its database.
  Every subscriber and every delivery belongs to that library, and an
  event never crosses to another one. Data from another library arrives
  through a typed import that publishes ordinary local events on the
  receiving side, as catalog sync does.
- Late-join reconcile remains for instances deployed after an event was
  published, but is no longer how a non-running instance catches up.
- Decisions 1 to 3 needed no `plugin.capnp` change: `EventConsumer.event`
  already takes a batch and `PluginWorker.shutdown` already exists.
  Decision 4 appends `EventPublisher.read`, its request, page and reply
  structs, and `maxEventCursorBytes`.
- See [Entrypoints, events and the ABI](../plugins/abi.md#how-events-are-delivered),
  [Reading past events](../plugins/abi.md#reading-past-events-enveventsread)
  and [Durable job queue](../internals/jobs.md).
