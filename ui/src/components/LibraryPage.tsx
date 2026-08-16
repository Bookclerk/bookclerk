import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { RefreshCw, ScanSearch } from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import {
  TitleDetailModal,
  type TitleMetaSearchKind,
} from "@/components/TitleDetailModal";
import { titleDetailFromBook } from "@/lib/titleDetail";
import { BookRow } from "@/components/BookRow";
import { JobsStrip } from "@/components/JobsStrip";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchBooks,
  fetchJobs,
  fetchStatus,
  signOut,
  triggerAcquire,
  triggerScan,
  type AuthRole,
  type BookRecord,
  type JobInfo,
  type StatusResponse,
} from "@/lib/api";
import {
  FILTER_KINDS,
  SORT_OPTIONS,
  bookMatchesFilter,
  filterValuesForKind,
  sortBooks,
  type FilterKind,
  type SortKey,
} from "@/lib/libraryFilters";
import { cn, pageWidthClass } from "@/lib/utils";

const PAGE_SIZE = 40;

const selectClassName =
  "rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

/**
 * Library view — searchable, filterable owned titles with acquire actions.
 *
 * @param props - Logout, acquire permission, nav, and optional role.
 */
export function LibraryPage({
  onLogout,
  canAcquire,
  nav,
  role,
}: {
  onLogout: () => void;
  canAcquire: boolean;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [books, setBooks] = useState<BookRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  const [q, setQ] = useState("");
  const [debouncedQ, setDebouncedQ] = useState("");
  const [filterKind, setFilterKind] = useState<FilterKind>("all");
  const [filterValue, setFilterValue] = useState("");
  const [sortKey, setSortKey] = useState<SortKey>("title");
  const [selected, setSelected] = useState<BookRecord | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  const loadingRef = useRef(false);

  const serverStatus =
    filterKind === "status" && filterValue ? filterValue : "all";

  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedQ(q.trim()), 250);
    return () => window.clearTimeout(t);
  }, [q]);

  const loadPage = useCallback(
    async (offset: number, append: boolean) => {
      if (loadingRef.current) return;
      loadingRef.current = true;
      if (append) setLoadingMore(true);
      try {
        const booksRes = await fetchBooks({
          q: debouncedQ || undefined,
          status: serverStatus,
          limit: PAGE_SIZE,
          offset,
        });
        setBooks((prev) =>
          append ? [...prev, ...booksRes.books] : booksRes.books,
        );
        setTotal(booksRes.total);
        setError(null);
      } catch (err) {
        setError(err instanceof Error ? err.message : "Failed to load library");
      } finally {
        loadingRef.current = false;
        setLoadingMore(false);
      }
    },
    [debouncedQ, serverStatus],
  );

  const refreshMeta = useCallback(async () => {
    if (!canAcquire) return;
    try {
      const [statusRes, jobsRes] = await Promise.all([fetchStatus(), fetchJobs()]);
      setStatus(statusRes);
      setJobs(jobsRes);
    } catch {
      // operator-only endpoints; ignore for portal
    }
  }, [canAcquire]);

  const refresh = useCallback(async () => {
    await Promise.all([loadPage(0, false), refreshMeta()]);
  }, [loadPage, refreshMeta]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!canAcquire) return;
    const id = window.setInterval(() => void refreshMeta(), 4000);
    return () => window.clearInterval(id);
  }, [canAcquire, refreshMeta]);

  useEffect(() => {
    const el = sentinelRef.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      (entries) => {
        const hit = entries.some((e) => e.isIntersecting);
        if (!hit) return;
        if (books.length >= total) return;
        void loadPage(books.length, true);
      },
      { root: null, rootMargin: "200px", threshold: 0 },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, [books.length, total, loadPage]);

  const filterValueOptions = useMemo(
    () => filterValuesForKind(books, filterKind),
    [books, filterKind],
  );

  const visibleBooks = useMemo(() => {
    const filtered =
      filterKind === "all" || filterKind === "status" || !filterValue
        ? books
        : books.filter((book) =>
            bookMatchesFilter(book, { kind: filterKind, value: filterValue }),
          );
    return sortBooks(filtered, sortKey);
  }, [books, filterKind, filterValue, sortKey]);

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
      setSelected((prev) => (prev?.uuid === book.uuid ? { ...book, acquire_status: "queued" } : prev));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Acquire failed");
    } finally {
      setBusyKey(null);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={cn("flex flex-col gap-3", pageWidthClass)}>
          <AppTopBar
            nav={nav}
            onSignOut={onSignOut}
            actions={
              canAcquire ? (
                <>
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
                    <span className="sm:hidden">Acquire</span>
                    <span className="hidden sm:inline">Acquire pending</span>
                  </Button>
                </>
              ) : (
                <Button
                  variant="secondary"
                  onClick={() => void refresh()}
                  disabled={busyKey !== null}
                >
                  <RefreshCw className="h-4 w-4" />
                  Refresh
                </Button>
              )
            }
          />
          <div className="flex flex-col gap-2 lg:flex-row">
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Search title, author, series, tags…"
              className="lg:flex-1"
            />
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
              <select
                value={filterKind}
                onChange={(e) => {
                  setFilterKind(e.target.value as FilterKind);
                  setFilterValue("");
                }}
                className={selectClassName}
                aria-label="Filter by"
              >
                {FILTER_KINDS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <select
                value={filterValue}
                onChange={(e) => setFilterValue(e.target.value)}
                className={selectClassName}
                aria-label="Filter value"
                disabled={filterKind === "all" || filterValueOptions.length === 0}
              >
                <option value="">
                  {filterKind === "all" ? "Any value" : "Select value"}
                </option>
                {filterValueOptions.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <select
                value={sortKey}
                onChange={(e) => setSortKey(e.target.value as SortKey)}
                className={selectClassName}
                aria-label="Sort by"
              >
                {SORT_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    Sort: {opt.label}
                  </option>
                ))}
              </select>
            </div>
          </div>
          {error ? (
            <p className="text-sm font-medium text-brick" role="alert">
              {error}
            </p>
          ) : (
            <p className="text-xs text-ink/50">
              Showing {visibleBooks.length} of {total} loaded titles
              {loadingMore ? " · loading more…" : ""}
              {filterKind !== "all" && filterKind !== "status" && filterValue
                ? " · client filter on loaded pages"
                : ""}
            </p>
          )}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
      <main className={cn("w-full", pageWidthClass)}>
        <div className="animate-[rowIn_500ms_ease-out] bg-card shadow-[inset_0_1px_0_rgba(11,53,83,0.06)]">
          {visibleBooks.length === 0 ? (
            <p className="px-4 py-16 text-center text-sm text-ink/60">
              No books match this view.
              {canAcquire ? " Run a scan or adjust filters." : " Link a store under Accounts."}
            </p>
          ) : (
            visibleBooks.map((book) => (
              <BookRow
                key={book.uuid}
                book={book}
                busy={busyKey !== null}
                showAcquire={canAcquire}
                onOpen={setSelected}
                onAcquire={(b) => void onAcquireBook(b)}
              />
            ))
          )}
          <div ref={sentinelRef} className="h-8" aria-hidden />
        </div>
      </main>
      </div>

      {selected ? (
        <TitleDetailModal
          key={selected.uuid}
          detail={titleDetailFromBook(selected)}
          busy={busyKey !== null}
          showAcquire={canAcquire}
          onClose={() => setSelected(null)}
          onAcquire={() => void onAcquireBook(selected)}
          onMetaSearch={(kind: TitleMetaSearchKind, value: string) => {
            setSelected(null);
            setFilterKind(kind);
            setFilterValue(value);
            setQ("");
            if (kind === "series") {
              setSortKey("series");
            }
          }}
        />
      ) : null}

      {canAcquire ? (
        <JobsStrip status={status} jobs={jobs} onChanged={() => void refreshMeta()} />
      ) : null}
      <style>{`
        @keyframes rowIn {
          from { opacity: 0; transform: translateY(6px); }
          to { opacity: 1; transform: translateY(0); }
        }
      `}</style>
    </div>
  );
}
