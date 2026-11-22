import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { RefreshCw, ScanSearch } from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { ContinueListening } from "@/components/ContinueListening";
import { LibraryTagsEditor } from "@/components/LibraryTagsEditor";
import { TitleDetailModal, type TitleMetaSearchKind } from "@/components/TitleDetailModal";
import { ConvertBookPanel } from "@/components/ConvertBookPanel";
import { titleDetailFromWork } from "@/lib/titleDetail";
import { WorkRow } from "@/components/WorkRow";
import { JobsStrip } from "@/components/JobsStrip";
import { WaveformThrobber } from "@/components/WaveformThrobber";
import { WindowBooksSection } from "@/components/WindowBooksSection";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  acquirableEditions,
  fetchEventDeliveries,
  fetchFacetValues,
  fetchJobs,
  fetchStatus,
  fetchWork,
  fetchWorks,
  signOut,
  storedEditions,
  triggerAcquire,
  triggerScan,
  type AcquireStatus,
  type AuthRole,
  type EventDeliveryInfo,
  type JobInfo,
  type MediaKind,
  type StatusResponse,
  type WorkDetail,
  type WorkFacet,
  type WorkSort,
  type WorkSummary,
} from "@/lib/api";
import {
  FACET_OPTIONS,
  facetQuery,
  metaSearchQuery,
  readFacetValues,
  readWorks,
} from "@/lib/libraryWorks";
import { canChangeBranchSharing } from "@/lib/join";
import { isDesktopWindow } from "@/lib/desktop";
import { useLiveRefresh } from "@/lib/live";
import { playableEdition } from "@/lib/playTarget";
import { playWork } from "@/lib/webPlayer";
import { cn, pageWidthClass } from "@/lib/utils";

const PAGE_SIZE = 40;

/** What a title load does; see `loadWorks` in {@link LibraryPage}. */
type LoadKind = "replace" | "reload" | "append";

/** Acquire-status filter options (`all` sends none). */
const STATUS_OPTIONS: { value: AcquireStatus | "all"; label: string }[] = [
  { value: "all", label: "Any status" },
  { value: "not_acquired", label: "Not acquired" },
  { value: "queued", label: "Queued" },
  { value: "downloading", label: "Downloading" },
  { value: "acquired", label: "Acquired" },
  { value: "error", label: "Error" },
];

/** Media-kind filter options (`all` sends none). */
const MEDIA_OPTIONS: { value: MediaKind | "all"; label: string }[] = [
  { value: "all", label: "Any format" },
  { value: "audio", label: "Audio" },
  { value: "ebook", label: "Ebook" },
  { value: "print", label: "Print" },
];

/** Page orders; a search orders by relevance instead. */
const SORT_OPTIONS: { value: WorkSort; label: string }[] = [
  { value: "title", label: "Title" },
  { value: "author", label: "Author" },
  { value: "series", label: "Series" },
  { value: "purchased", label: "Purchase date" },
  { value: "added", label: "Date added" },
  { value: "length", label: "Duration" },
];

const selectClassName =
  "rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

/**
 * Library view — the library's works, searchable and filterable, with
 * acquire actions. Each book or podcast is one row however many editions
 * and accounts hold it.
 *
 * @param props - Logout, acquire permission, nav, optional role, and
 *   whether a patron elevated to the operator.
 */
export function LibraryPage({
  onLogout,
  canAcquire,
  nav,
  role,
  elevated = false,
}: {
  onLogout: () => void;
  canAcquire: boolean;
  nav: AppNavProps;
  role?: AuthRole;
  /** Whether the caller is a patron elevated to the operator. */
  elevated?: boolean;
}) {
  const [works, setWorks] = useState<WorkSummary[]>([]);
  // False until the first load answers, so the list shows that it is
  // loading rather than "no books".
  const [loaded, setLoaded] = useState(false);
  // Cursor of the page after those shown; null once the last is shown.
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [status, setStatus] = useState<StatusResponse | null>(null);
  const [jobs, setJobs] = useState<JobInfo[]>([]);
  const [deliveries, setDeliveries] = useState<EventDeliveryInfo[]>([]);
  const [q, setQ] = useState("");
  const [debouncedQ, setDebouncedQ] = useState("");
  const [statusFilter, setStatusFilter] = useState<AcquireStatus | "all">("all");
  const [mediaFilter, setMediaFilter] = useState<MediaKind | "all">("all");
  const [sortKey, setSortKey] = useState<WorkSort>("title");
  const [facet, setFacet] = useState<WorkFacet | "all">("all");
  // The chosen value of `facet`; null until one is chosen.
  const [facetValue, setFacetValue] = useState<string | null>(null);
  // The menu of `facet`'s values, with the facet it was read for.
  // The filter in force: a facet counts once a value is chosen, so picking
  // only the facet does not reload the list.
  const appliedFacet = facetValue === null ? "all" : facet;
  const appliedValue = appliedFacet === "all" ? null : facetValue;
  // Bumped by Refresh to read the facet menu again.
  const [facetReads, setFacetReads] = useState(0);
  const [facetMenu, setFacetMenu] = useState<{
    facet: WorkFacet;
    values: { value: string; label: string }[];
  } | null>(null);
  const [selected, setSelected] = useState<WorkSummary | null>(null);
  // The detail read for an opened work, with the id it was read for.
  const [loadedDetail, setLoadedDetail] = useState<WorkDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const sentinelRef = useRef<HTMLDivElement | null>(null);
  // One load runs at a time; a request made meanwhile waits in
  // `pendingRef` and runs when it finishes (see `loadWorks`).
  const loadingRef = useRef(false);
  const pendingRef = useRef<"replace" | "reload" | null>(null);
  // Bumped by every load that replaces the list, so an answer for a
  // previous search or filter never lands on top of it.
  const generationRef = useRef(0);
  // Works shown and the cursor after them, kept in step with every write
  // so the next load reads from the right place before React re-renders.
  const loadedRef = useRef(0);
  const cursorRef = useRef<string | null>(null);
  const loadWorksRef = useRef<(kind: LoadKind) => Promise<void>>(async () => {});
  // Bumped after each successful load so the scroll sentinel checks again
  // whether it is still in view (the list may not have grown).
  const [sentinelCheck, setSentinelCheck] = useState(0);

  useEffect(() => {
    const t = window.setTimeout(() => setDebouncedQ(q.trim()), 250);
    return () => window.clearTimeout(t);
  }, [q]);

  // Reads the value menu of the chosen facet, again on every Refresh (so a
  // failed read can be retried).
  useEffect(() => {
    if (facet === "all") return;
    let current = true;
    readFacetValues(fetchFacetValues, facet)
      .then((values) => {
        if (current) setFacetMenu({ facet, values });
      })
      .catch((err: unknown) => {
        if (current) setError(err instanceof Error ? err.message : "Failed to load filter values");
      });
    return () => {
      current = false;
    };
  }, [facet, facetReads]);
  const facetValues = facetMenu && facetMenu.facet === facet ? facetMenu.values : null;

  /**
   * Loads works for the current search, filters and order.
   *
   * `replace` reads the first page (a new search or filter, or an explicit
   * refresh) and supersedes a load in flight. `reload` re-reads the works
   * already shown (at least one page), keeping how far the list was
   * scrolled; it waits for a load in flight instead of discarding it.
   * `append` reads the next page and is dropped while another load runs
   * (the sentinel asks again afterwards).
   */
  const loadWorks = useCallback(
    async (kind: LoadKind) => {
      if (loadingRef.current) {
        if (kind === "replace") {
          ++generationRef.current;
          pendingRef.current = "replace";
        } else if (kind === "reload" && pendingRef.current === null) {
          pendingRef.current = "reload";
        }
        return;
      }
      if (kind === "append" && cursorRef.current === null) return;
      loadingRef.current = true;
      const generation = kind === "append" ? generationRef.current : ++generationRef.current;
      const wanted =
        kind === "reload"
          ? Math.max(1, Math.ceil(loadedRef.current / PAGE_SIZE)) * PAGE_SIZE
          : PAGE_SIZE;
      if (kind === "append") setLoadingMore(true);
      let succeeded = false;
      try {
        const read = await readWorks(
          fetchWorks,
          {
            q: debouncedQ || undefined,
            sort: sortKey,
            status: statusFilter,
            media_kind: mediaFilter === "all" ? undefined : mediaFilter,
            ...facetQuery(appliedFacet, appliedValue),
          },
          wanted,
          kind === "append" ? (cursorRef.current ?? undefined) : undefined,
          () => generation === generationRef.current,
        );
        if (generation !== generationRef.current) return;
        if (kind === "append") {
          loadedRef.current += read.works.length;
          setWorks((prev) => [...prev, ...read.works]);
        } else {
          loadedRef.current = read.works.length;
          setWorks(read.works);
        }
        cursorRef.current = read.next;
        setNextCursor(read.next);
        setLoaded(true);
        setError(null);
        succeeded = true;
      } catch (err) {
        if (generation !== generationRef.current) return;
        setError(err instanceof Error ? err.message : "Failed to load library");
        setLoaded(true);
      } finally {
        loadingRef.current = false;
        setLoadingMore(false);
        const next = pendingRef.current;
        pendingRef.current = null;
        if (next) {
          // The latest search and filter (a replace may have been asked
          // for by a newer render).
          void loadWorksRef.current(next);
        } else if (succeeded) {
          // Not after a failure, which would retry in a loop.
          setSentinelCheck((n) => n + 1);
        }
      }
    },
    [debouncedQ, sortKey, statusFilter, mediaFilter, appliedFacet, appliedValue],
  );
  // Declared before the effects that load, so a queued load runs the
  // version for the current search and filter.
  useEffect(() => {
    loadWorksRef.current = loadWorks;
  }, [loadWorks]);

  const reloadWorks = useCallback(() => loadWorks("reload"), [loadWorks]);

  const refreshMeta = useCallback(async () => {
    if (!canAcquire) return;
    try {
      const [statusRes, jobsRes, deliveriesRes] = await Promise.all([
        fetchStatus(),
        fetchJobs(),
        fetchEventDeliveries(),
      ]);
      setStatus(statusRes);
      setJobs(jobsRes);
      setDeliveries(deliveriesRes);
    } catch {
      // operator-only endpoints; ignore for portal
    }
  }, [canAcquire]);

  const refresh = useCallback(async () => {
    setFacetReads((n) => n + 1);
    await Promise.all([loadWorks("replace"), refreshMeta()]);
  }, [loadWorks, refreshMeta]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Job and delivery strips follow the live socket; while it is down they
  // poll as before. While it is up a slow poll still runs, so a refresh
  // that failed once (it keeps the last snapshot) is retried even when no
  // watched topic changes again.
  useLiveRefresh(["jobs", "deliveries", "library"], refreshMeta, {
    pollMs: 4000,
    openPollMs: 30_000,
    enabled: canAcquire,
  });
  // Works follow the `library` topic for everyone who can browse them (a
  // scan or acquire on any node), keeping the search, filter and scroll.
  // While the socket is down the list is not polled, as before; the
  // reconnect refreshes it. While it is up a slow reconciliation runs: a
  // change stamped earlier than the newest one the socket compared (a
  // write that committed late, or a shared node whose clock runs behind)
  // does not move the revision.
  useLiveRefresh(["library"], reloadWorks, { pollMs: 0, openPollMs: 120_000 });

  useEffect(() => {
    const el = sentinelRef.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      (entries) => {
        const hit = entries.some((e) => e.isIntersecting);
        if (!hit) return;
        if (cursorRef.current === null) return;
        void loadWorks("append");
      },
      { root: null, rootMargin: "200px", threshold: 0 },
    );
    // A new observer reports whether the sentinel is in view right away,
    // so re-creating it after each load keeps pagination going.
    obs.observe(el);
    return () => obs.disconnect();
  }, [works.length, nextCursor, loadWorks, sentinelCheck]);

  // The open work's detail, read when a row is opened; a detail read for
  // another work is never shown.
  const selectedId = selected?.id ?? null;
  const selectedDetail = loadedDetail?.id === selectedId ? loadedDetail : null;
  useEffect(() => {
    if (selectedId === null) return;
    let cancelled = false;
    void (async () => {
      try {
        const detail = await fetchWork(selectedId);
        if (!cancelled) setLoadedDetail(detail);
      } catch (err) {
        if (cancelled) return;
        setError(err instanceof Error ? err.message : "Failed to load the title");
        setSelected(null);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedId]);

  const pendingCount = useMemo(
    () => status?.pending ?? works.filter((w) => w.acquire_status === "not_acquired").length,
    [status, works],
  );

  // The operator is no library user, so it has nothing to play as. In the
  // desktop window a patron who elevated keeps listening as themselves.
  const canListen = (role != null && role !== "operator") || (isDesktopWindow() && elevated);

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

  /** Queues every held edition of `work` that is not acquired yet. */
  async function onAcquireWork(work: WorkSummary, known?: WorkDetail | null) {
    setBusyKey(work.id);
    try {
      const detail = known ?? (await fetchWork(work.id));
      const editions = acquirableEditions(detail);
      if (editions.length === 0) {
        setError(`Nothing of “${work.title}” is waiting to be acquired.`);
        return;
      }
      await triggerAcquire({ editions });
      await refresh();
      setSelected((prev) => (prev?.id === work.id ? { ...prev, acquire_status: "queued" } : prev));
    } catch (err) {
      setError(err instanceof Error ? err.message : "Acquire failed");
    } finally {
      setBusyKey(null);
    }
  }

  /** Plays the audio of `work`, saying so when none is on this server. */
  async function onPlayWork(work: WorkSummary, known?: WorkDetail | null) {
    try {
      const refusal = await playWork(work.id, known);
      setError(refusal);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not play this book");
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
          <div className="flex flex-col gap-2">
            <Input
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder="Search title, author:, narrator:, series:, asin:…"
            />
            <div className="grid grid-cols-1 gap-2 sm:grid-cols-3 lg:grid-cols-5">
              <select
                value={facet}
                onChange={(e) => {
                  setFacet(e.target.value as WorkFacet | "all");
                  setFacetValue(null);
                }}
                className={selectClassName}
                aria-label="Filter by"
              >
                {FACET_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.value === "all" ? opt.label : `Filter: ${opt.label}`}
                  </option>
                ))}
              </select>
              {facet !== "all" ? (
                <select
                  value={facetValue ?? ""}
                  onChange={(e) => setFacetValue(e.target.value === "" ? null : e.target.value)}
                  className={selectClassName}
                  aria-label="Filter value"
                  disabled={facetValues === null}
                >
                  <option value="">{facetValues === null ? "Loading…" : "Any"}</option>
                  {(facetValues ?? []).map((opt) => (
                    <option key={opt.value} value={opt.value}>
                      {opt.label}
                    </option>
                  ))}
                </select>
              ) : null}
              <select
                value={statusFilter}
                onChange={(e) => setStatusFilter(e.target.value as AcquireStatus | "all")}
                className={selectClassName}
                aria-label="Acquire status"
              >
                {STATUS_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <select
                value={mediaFilter}
                onChange={(e) => setMediaFilter(e.target.value as MediaKind | "all")}
                className={selectClassName}
                aria-label="Format"
              >
                {MEDIA_OPTIONS.map((opt) => (
                  <option key={opt.value} value={opt.value}>
                    {opt.label}
                  </option>
                ))}
              </select>
              <select
                value={debouncedQ ? "relevance" : sortKey}
                onChange={(e) => setSortKey(e.target.value as WorkSort)}
                className={selectClassName}
                aria-label="Sort by"
                disabled={Boolean(debouncedQ)}
              >
                {debouncedQ ? <option value="relevance">Sort: Best match</option> : null}
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
              Showing {works.length} {works.length === 1 ? "title" : "titles"}
              {nextCursor !== null ? " · scroll for more" : ""}
              {loadingMore ? " · loading more…" : ""}
            </p>
          )}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
        <main className={cn("w-full", pageWidthClass)}>
          {/* On a window: the books the libraries it joined hold for its person. */}
          {canChangeBranchSharing(role) ? <WindowBooksSection query={debouncedQ} /> : null}
          {/* Only on the unfiltered library: a search is for finding something else. */}
          {canListen && !debouncedQ && appliedFacet === "all" ? <ContinueListening /> : null}
          <div className="animate-[rowIn_500ms_ease-out] bg-card shadow-[inset_0_1px_0_rgba(11,53,83,0.06)]">
            {works.length === 0 && !loaded ? (
              <div className="flex justify-center px-4 py-16" role="status">
                <WaveformThrobber size="sm" />
                <span className="sr-only">Loading the library…</span>
              </div>
            ) : works.length === 0 && nextCursor === null ? (
              <p className="px-4 py-16 text-center text-sm text-ink/60">
                No books match this view.
                {canAcquire ? " Run a scan or adjust filters." : " Link a store under Accounts."}
              </p>
            ) : (
              works.map((work) => (
                <WorkRow
                  key={work.id}
                  work={work}
                  busy={busyKey !== null}
                  showAcquire={canAcquire}
                  onOpen={setSelected}
                  onPlay={canListen ? (w) => void onPlayWork(w) : undefined}
                  onAcquire={(w) => void onAcquireWork(w)}
                />
              ))
            )}
            <div ref={sentinelRef} className="h-8" aria-hidden />
          </div>
        </main>
      </div>

      {selected && selectedDetail ? (
        <TitleDetailModal
          key={selected.id}
          detail={titleDetailFromWork(selectedDetail, selected.acquire_status)}
          busy={busyKey !== null}
          showAcquire={canAcquire}
          onClose={() => {
            setSelected(null);
            setLoadedDetail(null);
          }}
          onAcquire={() => void onAcquireWork(selected, selectedDetail)}
          onPlay={
            canListen && playableEdition(selectedDetail)
              ? () => void onPlayWork(selected, selectedDetail)
              : undefined
          }
          extra={
            role === "operator" || role === "owner" || role === "administrator" ? (
              <>
                <LibraryTagsEditor workId={selected.id} />
                {storedEditions(selectedDetail).length > 0 ? (
                  <ConvertBookPanel
                    editions={storedEditions(selectedDetail)}
                    onQueued={() => void refreshMeta()}
                  />
                ) : null}
              </>
            ) : undefined
          }
          onMetaSearch={(kind: TitleMetaSearchKind, value: string) => {
            setSelected(null);
            setLoadedDetail(null);
            if (kind === "genres") {
              // Genres are not in the search index: filter by the genre.
              setQ("");
              setFacet("genre");
              setFacetValue(value);
            } else {
              setQ(metaSearchQuery(kind, value));
            }
          }}
        />
      ) : null}

      {canAcquire ? (
        <JobsStrip
          status={status}
          jobs={jobs}
          deliveries={deliveries}
          onChanged={() => void refreshMeta()}
        />
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
