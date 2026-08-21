import type {
  EventDeliveryInfo,
  JobInfo,
  StatusResponse,
} from "@/lib/api";
import {
  ackEventDelivery,
  cancelEventDelivery,
  cancelJob,
  resumeEventDelivery,
  retryEventDelivery,
} from "@/lib/api";

/**
 * Compact status counters and recent jobs/events strip for Library.
 *
 * @param props - Status payload, job list, deliveries, and optional refresh.
 */
export function JobsStrip({
  status,
  jobs,
  deliveries,
  onChanged,
}: {
  status: StatusResponse | null;
  jobs: JobInfo[];
  deliveries: EventDeliveryInfo[];
  onChanged?: () => void;
}) {
  const recent = jobs.slice(0, 6);
  const eventRows = deliveries
    .filter(
      (d) =>
        d.state === "dead_letter" ||
        d.state === "running" ||
        (d.state === "pending" && d.hasCheckpoint),
    )
    .slice(0, 6);
  const events = status?.events;
  return (
    <aside className="border-t border-ink/10 bg-card px-4 py-2 text-xs text-ink/70 backdrop-blur-sm">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        {status ? (
          <>
            <span>
              <strong className="text-ink">{status.books}</strong> books
            </span>
            <span>
              <strong className="text-ink">{status.pending}</strong> pending
            </span>
            <span>
              <strong className="text-ink">{status.in_progress}</strong> in
              progress
            </span>
            <span>
              <strong className="text-brick">{status.error}</strong> errors
            </span>
            <span className="text-ink/45">{status.storage_backend}</span>
            {events ? (
              <span>
                events{" "}
                <strong className="text-ink">{events.pending}</strong> pending
                {" / "}
                <strong className="text-ink">{events.running}</strong> running
                {" / "}
                <strong className="text-ink">{events.suspended}</strong>{" "}
                suspended
                {" / "}
                <strong className="text-brick">{events.dead_letter}</strong>{" "}
                dead
                {events.oldest_pending_age_secs != null
                  ? ` · oldest ${events.oldest_pending_age_secs}s`
                  : ""}
                {events.dispatch_latency_ms_avg != null
                  ? ` · dispatch ${events.dispatch_latency_ms_avg}ms`
                  : ""}
                {events.handler_latency_ms_avg != null
                  ? ` · handler ${events.handler_latency_ms_avg}ms`
                  : ""}
                {events.retries_total
                  ? ` · retries ${events.retries_total}`
                  : ""}
                {events.suspensions_total
                  ? ` · suspends ${events.suspensions_total}`
                  : ""}
              </span>
            ) : null}
          </>
        ) : (
          <span>Loading status…</span>
        )}
      </div>
      {recent.length > 0 ? (
        <ul className="mt-1.5 space-y-0.5">
          {recent.map((job) => (
            <li
              key={job.id}
              className="flex items-center justify-between gap-2 font-mono text-[11px]"
            >
              <span className="truncate">
                {job.id} · {job.kind} · {job.status}
                {job.progress
                  ? ` · ${job.progress}`
                  : job.detail
                    ? ` · ${job.detail}`
                    : ""}
                {job.attempt_count
                  ? ` · try ${job.attempt_count}/${job.max_attempts ?? "?"}`
                  : ""}
              </span>
              {job.status === "pending" || job.status === "running" ? (
                <button
                  type="button"
                  className="shrink-0 text-brick hover:underline"
                  onClick={() => {
                    void cancelJob(job.id).then(() => onChanged?.());
                  }}
                >
                  cancel
                </button>
              ) : null}
            </li>
          ))}
        </ul>
      ) : null}
      {eventRows.length > 0 ? (
        <ul className="mt-1.5 space-y-0.5">
          {eventRows.map((row) => (
            <li
              key={row.id}
              className="flex items-center justify-between gap-2 font-mono text-[11px]"
            >
              <span className="truncate">
                {row.id} · {row.pluginId} · {row.state}
                {row.attemptCount ? ` · try ${row.attemptCount}` : ""}
                {row.errorMessage ? ` · ${row.errorMessage}` : ""}
              </span>
              <span className="flex shrink-0 gap-2">
                {row.state === "dead_letter" ? (
                  <>
                    <button
                      type="button"
                      className="text-ink hover:underline"
                      onClick={() => {
                        void retryEventDelivery(row.id).then(() =>
                          onChanged?.(),
                        );
                      }}
                    >
                      retry
                    </button>
                    <button
                      type="button"
                      className="text-brick hover:underline"
                      onClick={() => {
                        void ackEventDelivery(row.id).then(() =>
                          onChanged?.(),
                        );
                      }}
                    >
                      ack
                    </button>
                  </>
                ) : null}
                {row.state === "running" ||
                (row.state === "pending" && !row.hasCheckpoint) ? (
                  <button
                    type="button"
                    className="text-brick hover:underline"
                    onClick={() => {
                      void cancelEventDelivery(row.id).then(() =>
                        onChanged?.(),
                      );
                    }}
                  >
                    cancel
                  </button>
                ) : null}
                {row.state === "pending" && row.hasCheckpoint ? (
                  <>
                    <button
                      type="button"
                      className="text-ink hover:underline"
                      onClick={() => {
                        void resumeEventDelivery(row.id).then(() =>
                          onChanged?.(),
                        );
                      }}
                    >
                      resume
                    </button>
                    <button
                      type="button"
                      className="text-brick hover:underline"
                      onClick={() => {
                        void cancelEventDelivery(row.id).then(() =>
                          onChanged?.(),
                        );
                      }}
                    >
                      cancel
                    </button>
                  </>
                ) : null}
              </span>
            </li>
          ))}
        </ul>
      ) : null}
    </aside>
  );
}
