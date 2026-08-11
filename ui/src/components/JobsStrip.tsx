import type { JobInfo, StatusResponse } from "@/lib/api";

/**
 * Compact status counters and recent jobs strip for Library.
 *
 * @param props - Status payload and job list.
 */
export function JobsStrip({
  status,
  jobs,
}: {
  status: StatusResponse | null;
  jobs: JobInfo[];
}) {
  const recent = jobs.slice(-6).reverse();
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
            <li key={job.id} className="truncate font-mono text-[11px]">
              {job.id} · {job.kind} · {job.status}
              {job.detail ? ` · ${job.detail}` : ""}
            </li>
          ))}
        </ul>
      ) : null}
    </aside>
  );
}
