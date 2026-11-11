# ADR: Event delivery follows the Workers model

- **Status:** Accepted (phase 1); phases 2 to 4 proposed
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

- An instance that is deployed but failing to start accumulates
  `pending` deliveries for the events it subscribes to. They drain once
  it runs; an operator who gives up on it undeploys it (its row expires
  within the 60 s catalog TTL) and cancels the waiting deliveries with
  `bookclerk events cancel`.
- Late-join reconcile remains for instances deployed after an event was
  published, but is no longer how a non-running instance catches up.
- No `plugin.capnp` change: `EventConsumer.event` already takes a batch
  and `PluginWorker.shutdown` already exists.
- See [Entrypoints, events and the ABI](../plugins/abi.md#how-events-are-delivered)
  and [Durable job queue](../internals/jobs.md).
