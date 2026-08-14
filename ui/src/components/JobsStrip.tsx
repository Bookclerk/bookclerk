import type { JobInfo, StatusResponse } from "@/lib/api";
import { cancelJob } from "@/lib/api";

/**
 * Compact status counters and recent jobs strip for Library.
 *
 * @param props - Status payload, job list, and optional refresh after cancel.
 */
export function JobsStrip({
  status,
  jobs,
  onChanged,
}: {
  status: StatusResponse | null;
  jobs: JobInfo[];
  onChanged?: () => void;
}) {
  const recent = jobs.slice(0, 6);
  return (
    <aside className="border-t border-ink/10 bg-white/40 px-4 py-2 text-xs text-ink/70 backdrop-blur-sm">
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
    </aside>
  );
}
