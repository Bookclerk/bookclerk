import { useCallback, useEffect, useMemo, useState } from "react";
import { LogOut, RefreshCw, ScanSearch } from "lucide-react";
import { BookRow } from "@/components/BookRow";
import { JobsStrip } from "@/components/JobsStrip";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchBooks,
  fetchJobs,
  fetchStatus,
  logout,
  triggerAcquire,
  triggerScan,
  type BookRecord,
  type JobInfo,
  type StatusResponse,
} from "@/lib/api";

const STATUS_OPTIONS = [
  { value: "all", label: "All statuses" },
  { value: "not_acquired", label: "Not acquired" },
  { value: "queued", label: "Queued" },
  { value: "downloading", label: "Downloading" },
  { value: "acquired", label: "Acquired" },
  { value: "error", label: "Error" },
] as const;

export function LibraryPage({ onLogout }: { onLogout: () => void }) {
  const [books, setBooks] = useState<BookRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  const [q, setQ] = useState("");
  const [debouncedQ, setDebouncedQ] = useState("");
  const [statusFilter, setStatusFilter] = useState("all");
  const [error, setError] = useState<string | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedQ(q.trim()), 250);
    return () => window.clearTimeout(t);
  }, [q]);

  const refresh = useCallback(async () => {
    try {
      const [booksRes, statusRes, jobsRes] = await Promise.all([
        fetchBooks({ q: debouncedQ || undefined, status: statusFilter }),
        fetchStatus(),
        fetchJobs(),
      ]);
      setBooks(booksRes.books);
      setTotal(booksRes.total);
      setStatus(statusRes);
      setJobs(jobsRes);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load library");
    }
  }, [debouncedQ, statusFilter]);

  useEffect(() => {
    void refresh();
    const id = window.setInterval(() => void refresh(), 4000);
    return () => window.clearInterval(id);
  }, [refresh]);

  const pendingCount = useMemo(
    () => status?.pending ?? books.filter((b) => b.acquire_status === "not_acquired").length,
    [status, books],
  );

  async function onScan() {
    setBusyKey("scan");
    try {
      await triggerScan();
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Scan failed");
    } finally {
      setBusyKey(null);
    }
  }

  async function onAcquirePending() {
    setBusyKey("acquire-all");
    try {
      await triggerAcquire({});
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Acquire failed");
    } finally {
      setBusyKey(null);
    }
  }

  async function onAcquireBook(book: BookRecord) {
    setBusyKey(book.uuid);
    try {
      await triggerAcquire({ uuid: book.uuid });
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Acquire failed");
    } finally {
      setBusyKey(null);
    }
  }

  async function onSignOut() {
    await logout();
    onLogout();
  }

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className="mx-auto flex max-w-6xl flex-col gap-3">
          <div className="flex items-center justify-between gap-3">
            <img
              src="/bookclerk-logo.svg"
              alt="Bookclerk"
              className="h-8 w-auto sm:h-9"
            />
            <div className="flex items-center gap-2">
              <Button
                variant="secondary"
                onClick={() => void onScan()}
                disabled={busyKey !== null}
              >
                <ScanSearch className="h-4 w-4" />
                Scan
              </Button>
              <Button
                onClick={() => void onAcquirePending()}
                disabled={busyKey !== null || pendingCount === 0}
                title="Acquire all pending"
              >
                <RefreshCw className="h-4 w-4" />
                Acquire pending
              </Button>
              <Button variant="ghost" onClick={() => void onSignOut()} aria-label="Sign out">
                <LogOut className="h-4 w-4" />
              </Button>
            </div>
          </div>
          <div className="flex flex-col gap-2 sm:flex-row">
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Search title, author, series, tags…"
              className="sm:flex-1"
            />
            <select
              value={statusFilter}
              onChange={(e) => setStatusFilter(e.target.value)}
              className="rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
            >
              {STATUS_OPTIONS.map((opt) => (
                <option key={opt.value} value={opt.value}>
                  {opt.label}
                </option>
              ))}
            </select>
          </div>
          {error ? (
            <p className="text-sm font-medium text-brick" role="alert">
              {error}
            </p>
          ) : (
            <p className="text-xs text-ink/50">
              Showing {books.length} of {total} titles
            </p>
          )}
        </div>
      </header>

      <main className="mx-auto w-full max-w-6xl flex-1 overflow-auto">
        <div className="animate-[rowIn_500ms_ease-out] bg-white/35 shadow-[inset_0_1px_0_rgba(11,53,83,0.06)]">
          {books.length === 0 ? (
            <p className="px-4 py-16 text-center text-sm text-ink/60">
              No books match this view. Run a scan or adjust filters.
            </p>
          ) : (
            books.map((book) => (
              <BookRow
                key={book.uuid}
                book={book}
                busy={busyKey !== null}
                onAcquire={(b) => void onAcquireBook(b)}
              />
            ))
          )}
        </div>
      </main>

      <JobsStrip status={status} jobs={jobs} />
      <style>{`
        @keyframes rowIn {
          from { opacity: 0; transform: translateY(6px); }
          to { opacity: 1; transform: translateY(0); }
        }
      `}</style>
    </div>
  );
}
