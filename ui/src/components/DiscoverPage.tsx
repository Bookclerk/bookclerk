import { useEffect, useMemo, useRef, useState } from "react";
import {
  ArrowDownAZ,
  ArrowLeft,
  ArrowUpAZ,
  Bookmark,
  Loader2,
  Search,
  SlidersHorizontal,
  Sparkles,
} from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { CoverThumb } from "@/components/CoverThumb";
import { DiscoverFilterRail } from "@/components/DiscoverFilterRail";
import { StarRating } from "@/components/StarRating";
import { StoreLogo } from "@/components/StoreLogo";
import { WaveformThrobber } from "@/components/WaveformThrobber";
import { useRegisterShelvesChangeListener } from "@/components/PreferencesDialog";
import {
  TitleDetailModal,
  titleDetailFromCatalog,
  type TitleDetail,
  type TitleMetaSearchKind,
} from "@/components/TitleDetailModal";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { cn, pageWidthClass } from "@/lib/utils";
import {
  applyTitleMeta,
  buildCatalogSearchFilters,
  CATALOG_LANGUAGE_ALL,
  CATALOG_SORT_OPTIONS,
  catalogFilterValues,
  catalogLanguageSearchOpts,
  catalogLanguageSelectOptions,
  catalogTitleFromHit,
  catalogTitleFromRec,
  defaultSortDirFor,
  discoverSearchFromMeta,
  formatShelfBestPrice,
  formatSeriesLabel,
  preferCatalogLanguageOrder,
  preferredCatalogLanguage,
  splitMetaList,
  storeLabel,
  titleNeedsMeta,
  titleNeedsShelfMeta,
  type CatalogFilterKind,
  type CatalogSortKey,
  type CatalogTitle,
  type RuntimeBucket,
} from "@/lib/catalogTitle";
import {
  createWishlistItem,
  fetchDiscoverFeed,
  fetchPreferences,
  fetchPurchaseHints,
  fetchTitleMeta,
  fetchTitleMetaBatch,
  fetchWishlist,
  removeWishlistItem,
  searchCatalog,
  signOut,
  userFacingApiError,
  type AuthRole,
  type CatalogSearchHit,
  type CatalogSortDir,
  type DiscoverFeed,
  type DiscoverShelf,
  type PurchaseHint,
  type Recommendation,
  type TitleMeta,
  type TitleRequest,
  type UserPreferences,
} from "@/lib/api";
import { formatDuration } from "@/lib/libraryFilters";
import {
  takeQueuedDiscoverSearch,
  type DiscoverSearchHandoff,
} from "@/lib/routes";
import { descriptionPlainText } from "@/lib/safeHtml";

const SHELF_CHUNK = 8;
const SHELVES_INITIAL = 6;
const RESULTS_PAGE_SIZE = 24;

const selectClassName =
  "rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

function normalizeIsbn(raw: string): string {
  return raw.replace(/[^0-9Xx]/g, "").toUpperCase();
}

function wishMatchesRec(req: TitleRequest, rec: Recommendation): boolean {
  if (req.status !== "open") return false;
  if (req.work_key && rec.work_key && req.work_key === rec.work_key) return true;
  if (req.work_key && rec.asin) {
    const key = `asin:${rec.asin.toUpperCase()}`;
    if (req.work_key === key) return true;
  }
  if (req.work_key && rec.isbn) {
    const key = `isbn:${normalizeIsbn(rec.isbn)}`;
    if (req.work_key === key) return true;
  }
  if (rec.asin && req.asin && rec.asin.toUpperCase() === req.asin.toUpperCase()) {
    return true;
  }
  if (rec.isbn && req.isbn && normalizeIsbn(rec.isbn) === normalizeIsbn(req.isbn)) {
    return true;
  }
  return req.title.trim().toLowerCase() === rec.title.trim().toLowerCase();
}

function wishMatchesHit(req: TitleRequest, hit: CatalogSearchHit): boolean {
  if (req.status !== "open") return false;
  if (req.work_key && hit.work_key && req.work_key === hit.work_key) return true;
  if (hit.asin && req.asin && hit.asin.toUpperCase() === req.asin.toUpperCase()) {
    return true;
  }
  if (hit.isbn && req.isbn && normalizeIsbn(hit.isbn) === normalizeIsbn(req.isbn)) {
    return true;
  }
  return req.title.trim().toLowerCase() === hit.title.trim().toLowerCase();
}

function wishlistUuidForHit(wishlist: TitleRequest[], hit: CatalogSearchHit): string | null {
  return wishlist.find((r) => wishMatchesHit(r, hit))?.uuid ?? null;
}

function wishlistUuidForRec(wishlist: TitleRequest[], rec: Recommendation): string | null {
  return wishlist.find((r) => wishMatchesRec(r, rec))?.uuid ?? null;
}

function parseSortKey(raw: string | undefined): CatalogSortKey {
  switch (raw) {
    case "popularity":
    case "rating":
    case "title":
    case "author":
    case "price":
    case "length":
      return raw;
    default:
      return "relevance";
  }
}

export function DiscoverPage({
  onLogout,
  nav,
  role,
}: {
  onLogout: () => void;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [feed, setFeed] = useState<DiscoverFeed>({ shelves: [] });
  const [wishlist, setWishlist] = useState<TitleRequest[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [visibleShelves, setVisibleShelvesCount] = useState(SHELVES_INITIAL);
  const shelfSentinelRef = useRef<HTMLDivElement | null>(null);

  const [searchQ, setSearchQ] = useState("");
  const [suggestions, setSuggestions] = useState<CatalogSearchHit[]>([]);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchBusy, setSearchBusy] = useState(false);
  const searchWrapRef = useRef<HTMLDivElement | null>(null);
  const searchSeq = useRef(0);

  const [panel, setPanel] = useState<"shelves" | "results">("shelves");
  const [resultsQ, setResultsQ] = useState("");
  const [resultsField, setResultsField] = useState<
    "author" | "narrator" | "series" | "genre" | undefined
  >(undefined);
  const [results, setResults] = useState<CatalogSearchHit[]>([]);
  const [resultMeta, setResultMeta] = useState<Record<string, TitleMeta>>({});
  const [resultsBusy, setResultsBusy] = useState(false);
  const [resultsLoadingMore, setResultsLoadingMore] = useState(false);
  const [resultsHasMore, setResultsHasMore] = useState(false);
  const [resultsCursor, setResultsCursor] = useState<string | null>(null);
  const [sortKey, setSortKey] = useState<CatalogSortKey>("relevance");
  const [sortDir, setSortDir] = useState<CatalogSortDir>("desc");
  const [hideVirtualVoice, setHideVirtualVoice] = useState(true);
  const [filterLanguage, setFilterLanguage] = useState(() =>
    preferredCatalogLanguage(),
  );
  const [filterAuthors, setFilterAuthors] = useState<string[]>([]);
  const [filterNarrators, setFilterNarrators] = useState<string[]>([]);
  const [filterSeries, setFilterSeries] = useState<string[]>([]);
  const [filterGenres, setFilterGenres] = useState<string[]>([]);
  const [excludedSources, setExcludedSources] = useState<string[]>([]);
  const [minRating, setMinRating] = useState<number | null>(null);
  const [runtimeBucket, setRuntimeBucket] = useState<RuntimeBucket>("any");
  const [filtersOpen, setFiltersOpen] = useState(false);
  const [prefsReady, setPrefsReady] = useState(false);
  const discoverPrefsRef = useRef<UserPreferences | null>(null);
  const [selected, setSelected] = useState<CatalogTitle | null>(null);
  const resultsSentinelRef = useRef<HTMLDivElement | null>(null);
  const resultsSeenKeys = useRef<Set<string>>(new Set());
  const resultsSeq = useRef(0);

  function applyDiscoverPrefs(prefs: UserPreferences) {
    setSortKey(parseSortKey(prefs.discover_sort));
    setSortDir(prefs.discover_sort_dir);
    setFilterLanguage(prefs.discover_language ?? preferredCatalogLanguage());
    setExcludedSources(prefs.discover_excluded_sources ?? []);
  }

  async function refreshFeed() {
    const [f, w] = await Promise.all([fetchDiscoverFeed(36), fetchWishlist()]);
    setFeed(f);
    setWishlist(w);
    setVisibleShelvesCount(SHELVES_INITIAL);
  }

  async function refresh() {
    setError(null);
    setBusy(true);
    try {
      await refreshFeed();
    } catch (err) {
      setError(
        userFacingApiError(err, "Couldn't load Discover shelves. Try again."),
      );
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const prefs = await fetchPreferences();
        if (cancelled) return;
        discoverPrefsRef.current = prefs;
        applyDiscoverPrefs(prefs);
      } catch {
        // Keep browser/language defaults when prefs fail.
      } finally {
        if (!cancelled) setPrefsReady(true);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!prefsReady) return;
    const queued = takeQueuedDiscoverSearch();
    if (queued) void runResultsSearch(queued.q, queued);
    // Prefs-ready handoff from Wishlist / other views.
    // eslint-disable-next-line react-hooks/exhaustive-deps -- intentional
  }, [prefsReady]);

  useRegisterShelvesChangeListener(() => {
    void refreshFeed();
  });

  useEffect(() => {
    const q = searchQ.trim();
    if (panel === "results" || q.length < 2) {
      setSuggestions([]);
      setSearchBusy(false);
      return;
    }
    const seq = ++searchSeq.current;
    setSearchBusy(true);
    const t = window.setTimeout(() => {
      void (async () => {
        try {
          const page = await searchCatalog(q, { page_size: 10 });
          if (seq !== searchSeq.current) return;
          setSuggestions(page.items);
          setSearchOpen(true);
        } catch {
          if (seq !== searchSeq.current) return;
          setSuggestions([]);
        } finally {
          if (seq === searchSeq.current) setSearchBusy(false);
        }
      })();
    }, 280);
    return () => window.clearTimeout(t);
  }, [searchQ, panel]);

  useEffect(() => {
    function onDocClick(e: MouseEvent) {
      if (!searchWrapRef.current?.contains(e.target as Node)) {
        setSearchOpen(false);
      }
    }
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, []);

  function handoffToFilterState(opts?: DiscoverSearchHandoff) {
    const prefs = discoverPrefsRef.current;
    const authors =
      opts?.authors ??
      (opts?.filterKind === "authors" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const narrators =
      opts?.narrators ??
      (opts?.filterKind === "narrators" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const series =
      opts?.series ??
      (opts?.filterKind === "series" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const genres =
      opts?.genres ??
      (opts?.filterKind === "genres" && opts.filterValue
        ? [opts.filterValue]
        : []);
    setFilterAuthors(authors);
    setFilterNarrators(narrators);
    setFilterSeries(series);
    setFilterGenres(genres);
    const excluded =
      opts?.excludeSources ??
      (opts?.filterKind === "sources" && opts.filterValue
        ? [] // legacy include-only; leave prefs exclusions
        : prefs?.discover_excluded_sources) ??
      excludedSources;
    setExcludedSources(excluded);
    setMinRating(opts?.minRating ?? null);
    setRuntimeBucket(opts?.runtimeBucket ?? "any");
    setHideVirtualVoice(opts?.hideVirtualVoice ?? true);
    setFilterLanguage(
      opts?.language ??
        prefs?.discover_language ??
        preferredCatalogLanguage(),
    );
    const nextSort = parseSortKey(opts?.sort ?? opts?.sortKey ?? prefs?.discover_sort);
    setSortKey(nextSort);
    setSortDir(
      opts?.sortDir ??
        prefs?.discover_sort_dir ??
        defaultSortDirFor(nextSort),
    );
    setResultsField(opts?.field);
  }

  async function enrichResultMeta(hits: CatalogSearchHit[]) {
    const sparse = hits
      .map((hit) => catalogTitleFromHit(hit))
      .filter(titleNeedsMeta);
    if (sparse.length === 0) return;
    try {
      const metas = await fetchTitleMetaBatch(
        sparse.map((t) => ({
          title: t.title,
          authors: t.authors,
          asin: t.asin,
          isbn: t.isbn,
          narrators: t.narrators,
          length_minutes: t.length_minutes,
        })),
      );
      setResultMeta((prev) => {
        const next = { ...prev };
        sparse.forEach((t, i) => {
          const meta = metas[i];
          if (meta) next[t.work_key] = meta;
        });
        return next;
      });
    } catch {
      // Enrichment is best-effort.
    }
  }

  async function runResultsSearch(q: string, opts?: DiscoverSearchHandoff) {
    const trimmed = q.trim();
    if (trimmed.length < 2) return;
    setSearchOpen(false);
    setSuggestions([]);
    setPanel("results");
    setResultsQ(trimmed);
    setSearchQ(trimmed);
    handoffToFilterState(opts);
    const prefs = discoverPrefsRef.current;
    const sort = parseSortKey(
      opts?.sort ?? opts?.sortKey ?? prefs?.discover_sort,
    );
    const dir: CatalogSortDir =
      opts?.sortDir ?? prefs?.discover_sort_dir ?? defaultSortDirFor(sort);
    const hideVV = opts?.hideVirtualVoice ?? true;
    const language =
      opts?.language ??
      prefs?.discover_language ??
      preferredCatalogLanguage();
    const langOpts = catalogLanguageSearchOpts(language);
    const authors =
      opts?.authors ??
      (opts?.filterKind === "authors" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const narrators =
      opts?.narrators ??
      (opts?.filterKind === "narrators" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const series =
      opts?.series ??
      (opts?.filterKind === "series" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const genres =
      opts?.genres ??
      (opts?.filterKind === "genres" && opts.filterValue
        ? [opts.filterValue]
        : []);
    const exclude =
      opts?.excludeSources ??
      prefs?.discover_excluded_sources ??
      [];
    const rating = opts?.minRating ?? null;
    const runtime = opts?.runtimeBucket ?? "any";
    const seq = ++resultsSeq.current;
    resultsSeenKeys.current = new Set();
    setResultsBusy(true);
    setResultsLoadingMore(false);
    setResults([]);
    setResultsCursor(null);
    setResultsHasMore(false);
    setResultMeta({});
    setError(null);
    try {
      const page = await searchCatalog(trimmed, {
        page_size: RESULTS_PAGE_SIZE,
        sort,
        sort_dir: dir,
        field: opts?.field,
        allLanguages: langOpts.allLanguages,
        filters: buildCatalogSearchFilters({
          authors,
          narrators,
          series,
          genres,
          excludeSources: exclude,
          languages: langOpts.languages,
          hideVirtualVoice: hideVV,
          minRating: rating,
          runtimeBucket: runtime,
        }),
      });
      if (seq !== resultsSeq.current) return;
      const items: CatalogSearchHit[] = [];
      for (const hit of page.items) {
        if (resultsSeenKeys.current.has(hit.work_key)) continue;
        resultsSeenKeys.current.add(hit.work_key);
        items.push(hit);
      }
      setResults(items);
      setResultsCursor(page.next_cursor ?? null);
      setResultsHasMore(Boolean(page.has_more && page.next_cursor));
      void enrichResultMeta(items);
    } catch (err) {
      if (seq !== resultsSeq.current) return;
      setResults([]);
      setResultMeta({});
      setError(
        userFacingApiError(
          err,
          "Couldn't search the catalog. Try again, or narrow your filters.",
        ),
      );
    } finally {
      if (seq === resultsSeq.current) setResultsBusy(false);
    }
  }

  async function loadMoreResults() {
    if (
      resultsBusy ||
      resultsLoadingMore ||
      !resultsHasMore ||
      !resultsCursor ||
      resultsQ.trim().length < 2
    ) {
      return;
    }
    const seq = resultsSeq.current;
    setResultsLoadingMore(true);
    setError(null);
    try {
      const langOpts = catalogLanguageSearchOpts(filterLanguage);
      const page = await searchCatalog(resultsQ, {
        page_size: RESULTS_PAGE_SIZE,
        cursor: resultsCursor,
        sort: sortKey,
        sort_dir: sortDir,
        field: resultsField,
        allLanguages: langOpts.allLanguages,
        filters: buildCatalogSearchFilters({
          authors: filterAuthors,
          narrators: filterNarrators,
          series: filterSeries,
          genres: filterGenres,
          excludeSources: excludedSources,
          languages: langOpts.languages,
          hideVirtualVoice,
          minRating,
          runtimeBucket,
        }),
      });
      if (seq !== resultsSeq.current) return;
      const appended: CatalogSearchHit[] = [];
      for (const hit of page.items) {
        if (resultsSeenKeys.current.has(hit.work_key)) continue;
        resultsSeenKeys.current.add(hit.work_key);
        appended.push(hit);
      }
      setResults((prev) => [...prev, ...appended]);
      setResultsCursor(page.next_cursor ?? null);
      setResultsHasMore(Boolean(page.has_more && page.next_cursor));
      void enrichResultMeta(appended);
    } catch (err) {
      if (seq !== resultsSeq.current) return;
      setError(
        userFacingApiError(
          err,
          "Couldn't load more results. Try narrowing with filters, or start a new search.",
        ),
      );
    } finally {
      if (seq === resultsSeq.current) setResultsLoadingMore(false);
    }
  }

  function refetchWithFilters(next: {
    sort?: CatalogSortKey;
    sortDir?: CatalogSortDir;
    hideVirtualVoice?: boolean;
    language?: string;
    authors?: string[];
    narrators?: string[];
    series?: string[];
    genres?: string[];
    excludeSources?: string[];
    minRating?: number | null;
    runtimeBucket?: RuntimeBucket;
  }) {
    void runResultsSearch(resultsQ, {
      q: resultsQ,
      field: resultsField,
      sort: next.sort ?? sortKey,
      sortDir: next.sortDir ?? sortDir,
      hideVirtualVoice: next.hideVirtualVoice ?? hideVirtualVoice,
      language: next.language ?? filterLanguage,
      authors: next.authors ?? filterAuthors,
      narrators: next.narrators ?? filterNarrators,
      series: next.series ?? filterSeries,
      genres: next.genres ?? filterGenres,
      excludeSources: next.excludeSources ?? excludedSources,
      minRating:
        next.minRating === undefined ? (minRating ?? undefined) : (next.minRating ?? undefined),
      runtimeBucket: next.runtimeBucket ?? runtimeBucket,
    });
  }

  async function onWishlistTitle(title: CatalogTitle | TitleDetail) {
    if (title.wishlist_uuid) return;
    setBusy(true);
    setError(null);
    try {
      const workKey =
        "work_key" in title && title.work_key ? title.work_key : undefined;
      const editions =
        "store_editions" in title ? title.store_editions ?? undefined : undefined;
      const created = await createWishlistItem({
        title: title.title,
        authors: title.authors ?? undefined,
        asin: title.asin ?? undefined,
        isbn: title.isbn ?? undefined,
        work_key: workKey || undefined,
        store_editions: editions,
        purchase_hints:
          "purchase_hints" in title ? title.purchase_hints ?? undefined : undefined,
        cover_url: title.cover_url ?? undefined,
        description: title.description ?? undefined,
        subtitle: title.subtitle ?? undefined,
        narrators: title.narrators ?? undefined,
        series: title.series ?? undefined,
        series_index: title.series_index ?? undefined,
        publisher: title.publisher ?? undefined,
        length_minutes: title.length_minutes ?? undefined,
        published_at: title.published_at ?? undefined,
        genres:
          ("genres" in title ? title.genres : null) ??
          ("categories" in title ? title.categories : null) ??
          undefined,
        language: title.language ?? undefined,
        notes: "Wishlisted from Discover",
      });
      await refreshFeed();
      setSelected((prev) =>
        prev &&
        (prev.work_key === workKey ||
          prev.title === title.title)
          ? { ...prev, wishlist_uuid: created.uuid }
          : prev,
      );
    } catch (err) {
      setError(
        userFacingApiError(err, "Couldn't add that title to your wishlist."),
      );
    } finally {
      setBusy(false);
    }
  }

  async function onRemoveWishlist(title: CatalogTitle | TitleDetail) {
    if (!title.wishlist_uuid) return;
    setBusy(true);
    setError(null);
    try {
      await removeWishlistItem(title.wishlist_uuid);
      await refreshFeed();
      setSelected((prev) =>
        prev && prev.wishlist_uuid === title.wishlist_uuid
          ? { ...prev, wishlist_uuid: null }
          : prev,
      );
    } catch (err) {
      setError(
        userFacingApiError(err, "Couldn't remove that title from your wishlist."),
      );
    } finally {
      setBusy(false);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  const shownShelves = feed.shelves.slice(0, visibleShelves);

  useEffect(() => {
    const el = shelfSentinelRef.current;
    if (!el || panel !== "shelves") return;
    const obs = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return;
        setVisibleShelvesCount((n) =>
          Math.min(n + SHELVES_INITIAL, feed.shelves.length),
        );
      },
      { rootMargin: "160px" },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, [feed.shelves.length, shownShelves.length, panel]);

  const resultTitles = useMemo(
    () =>
      results.map((hit) =>
        applyTitleMeta(
          catalogTitleFromHit(hit, wishlistUuidForHit(wishlist, hit)),
          resultMeta[hit.work_key],
        ),
      ),
    [results, wishlist, resultMeta],
  );

  const facetOptions = useMemo(() => {
    const withSeed = (kind: CatalogFilterKind, selected: string[]) => {
      const opts = catalogFilterValues(resultTitles, kind);
      const missing = selected.filter(
        (v) => !opts.some((o) => o.value === v),
      );
      return [
        ...missing.map((v) => ({ value: v, label: v })),
        ...opts,
      ];
    };
    return {
      authors: withSeed("authors", filterAuthors),
      narrators: withSeed("narrators", filterNarrators),
      series: withSeed("series", filterSeries),
      genres: withSeed("genres", filterGenres),
      sources: catalogFilterValues(resultTitles, "sources"),
    };
  }, [
    resultTitles,
    filterAuthors,
    filterNarrators,
    filterSeries,
    filterGenres,
  ]);

  useEffect(() => {
    if (panel !== "results" || !resultsHasMore) return;
    const el = resultsSentinelRef.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return;
        void loadMoreResults();
      },
      { rootMargin: "240px" },
    );
    obs.observe(el);
    return () => obs.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- sentinel reload when page state changes
  }, [
    panel,
    resultsHasMore,
    resultsCursor,
    resultsBusy,
    resultsLoadingMore,
    results.length,
  ]);

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={cn("flex flex-col gap-3", pageWidthClass)}>
          <AppTopBar
            nav={nav}
            onSignOut={onSignOut}
            actions={
              <Button
                variant="secondary"
                onClick={() => void refresh()}
                disabled={busy}
              >
                <Sparkles className="h-4 w-4" />
                Refresh
              </Button>
            }
          />

          <div ref={searchWrapRef} className="relative">
            <div className="relative">
              <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-ink/40" />
              <Input
                value={searchQ}
                onChange={(e) => {
                  setSearchQ(e.target.value);
                  if (panel === "shelves") setSearchOpen(true);
                }}
                onFocus={() => {
                  if (panel === "shelves" && suggestions.length > 0) setSearchOpen(true);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    void runResultsSearch(searchQ);
                  }
                  if (e.key === "Escape") setSearchOpen(false);
                }}
                placeholder="Search Audible, Libro.fm, Chirp, GraphicAudio… (Enter for results)"
                className={cn(
                  "h-11 pl-9",
                  searchBusy || resultsBusy ? "pr-10" : null,
                )}
                aria-label="Search store catalogs"
                aria-autocomplete="list"
                aria-expanded={searchOpen}
                aria-busy={searchBusy || resultsBusy}
              />
              {searchBusy || resultsBusy ? (
                <Loader2
                  className="pointer-events-none absolute right-3 top-1/2 h-4 w-4 -translate-y-1/2 animate-spin text-teal"
                  aria-hidden
                />
              ) : null}
            </div>
            {panel === "shelves" &&
            searchOpen &&
            (searchBusy || suggestions.length > 0 || searchQ.trim().length >= 2) ? (
              <ul
                className="absolute z-20 mt-1 max-h-80 w-full overflow-auto border border-ink/10 bg-paper shadow-lg"
                role="listbox"
              >
                {searchBusy && suggestions.length === 0 ? (
                  <li
                    className="flex items-center gap-2 px-3 py-3 text-sm text-ink/55"
                    role="status"
                    aria-live="polite"
                  >
                    <Loader2
                      className="h-4 w-4 shrink-0 animate-spin text-teal"
                      aria-hidden
                    />
                    Searching catalogs…
                  </li>
                ) : null}
                {!searchBusy && suggestions.length === 0 && searchQ.trim().length >= 2 ? (
                  <li className="px-3 py-2 text-sm text-ink/50">
                    No quick matches — press Enter for a full results page.
                  </li>
                ) : null}
                {suggestions.map((hit) => {
                  const already = wishlist.some((r) => wishMatchesHit(r, hit));
                  return (
                    <li
                      key={hit.work_key}
                      role="option"
                      className="flex items-center justify-between gap-2 border-b border-ink/5 px-3 py-2 last:border-0"
                    >
                      <button
                        type="button"
                        className="flex min-w-0 flex-1 items-center gap-3 text-left"
                        onClick={() => {
                          setSearchOpen(false);
                          setSelected(
                            catalogTitleFromHit(hit, wishlistUuidForHit(wishlist, hit)),
                          );
                        }}
                      >
                        <CoverThumb url={hit.cover_url} />
                        <div className="min-w-0">
                          <p className="truncate text-sm font-medium text-ink">{hit.title}</p>
                          <p className="truncate text-xs text-ink/50">
                            {hit.authors ?? "Unknown author"}
                            {formatSeriesLabel(hit.series, hit.series_index)
                              ? ` · ${formatSeriesLabel(hit.series, hit.series_index)}`
                              : ""}
                            {hit.sources.length
                              ? ` · ${hit.sources.map(storeLabel).join(", ")}`
                              : ""}
                          </p>
                        </div>
                      </button>
                      <Button
                        variant={already ? "secondary" : "ghost"}
                        className="h-8 shrink-0 gap-1 px-2 text-[11px]"
                        disabled={busy || already}
                        onClick={() =>
                          void onWishlistTitle(
                            catalogTitleFromHit(hit, wishlistUuidForHit(wishlist, hit)),
                          )
                        }
                      >
                        <Bookmark className={`h-3.5 w-3.5 ${already ? "fill-current" : ""}`} />
                        {already ? "Wishlisted" : "Wishlist"}
                      </Button>
                    </li>
                  );
                })}
                {searchQ.trim().length >= 2 ? (
                  <li className="border-t border-ink/10 px-3 py-2">
                    <button
                      type="button"
                      className="w-full text-left text-sm font-medium text-teal hover:underline"
                      onClick={() => void runResultsSearch(searchQ)}
                    >
                      See all results for “{searchQ.trim()}”
                    </button>
                  </li>
                ) : null}
              </ul>
            ) : null}
          </div>

          {panel === "results" ? (
            <div className="flex flex-col gap-2">
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  variant="ghost"
                  className="h-9 w-fit shrink-0 px-2"
                  onClick={() => {
                    setPanel("shelves");
                    setResults([]);
                    setResultsQ("");
                    setResultsCursor(null);
                    setResultsHasMore(false);
                    setFiltersOpen(false);
                  }}
                >
                  <ArrowLeft className="h-4 w-4" />
                  Shelves
                </Button>
                <select
                  value={sortKey}
                  onChange={(e) => {
                    const next = parseSortKey(e.target.value);
                    const nextDir = defaultSortDirFor(next);
                    setSortKey(next);
                    setSortDir(nextDir);
                    refetchWithFilters({ sort: next, sortDir: nextDir });
                  }}
                  className={selectClassName}
                  aria-label="Sort by"
                >
                  {CATALOG_SORT_OPTIONS.map((opt) => (
                    <option key={opt.value} value={opt.value}>
                      Sort: {opt.label}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className={cn(
                    "inline-flex h-9 items-center gap-1.5 rounded-md border px-3 text-sm shadow-sm",
                    sortKey === "relevance"
                      ? "cursor-not-allowed border-ink/10 bg-white/40 text-ink/40"
                      : "border-ink/15 bg-white/80 text-ink hover:bg-white",
                  )}
                  disabled={sortKey === "relevance"}
                  aria-label={
                    sortDir === "asc" ? "Sort ascending" : "Sort descending"
                  }
                  title={
                    sortKey === "relevance"
                      ? "Relevance uses store ranking order"
                      : sortDir === "asc"
                        ? "Ascending — click for descending"
                        : "Descending — click for ascending"
                  }
                  onClick={() => {
                    const next: CatalogSortDir =
                      sortDir === "asc" ? "desc" : "asc";
                    setSortDir(next);
                    refetchWithFilters({ sortDir: next });
                  }}
                >
                  {sortDir === "asc" ? (
                    <ArrowUpAZ className="h-4 w-4" />
                  ) : (
                    <ArrowDownAZ className="h-4 w-4" />
                  )}
                  <span className="hidden sm:inline">
                    {sortDir === "asc" ? "Asc" : "Desc"}
                  </span>
                </button>
                <select
                  value={filterLanguage}
                  onChange={(e) => {
                    const next = e.target.value;
                    setFilterLanguage(next);
                    refetchWithFilters({ language: next });
                  }}
                  className={selectClassName}
                  aria-label="Language"
                >
                  {catalogLanguageSelectOptions(preferredCatalogLanguage()).map(
                    (opt) => (
                      <option key={opt.value} value={opt.value}>
                        {opt.value === CATALOG_LANGUAGE_ALL
                          ? opt.label
                          : `Language: ${opt.label}`}
                      </option>
                    ),
                  )}
                </select>
                <button
                  type="button"
                  className={cn(
                    "h-9 rounded-md border px-3 text-sm shadow-sm",
                    hideVirtualVoice
                      ? "border-teal/40 bg-teal/10 text-ink"
                      : "border-ink/15 bg-white/80 text-ink/70",
                  )}
                  aria-pressed={hideVirtualVoice}
                  onClick={() => {
                    const next = !hideVirtualVoice;
                    setHideVirtualVoice(next);
                    refetchWithFilters({ hideVirtualVoice: next });
                  }}
                >
                  Hide Virtual Voice
                </button>
                <Button
                  variant="secondary"
                  className="h-9 lg:hidden"
                  aria-expanded={filtersOpen}
                  onClick={() => setFiltersOpen((v) => !v)}
                >
                  <SlidersHorizontal className="h-4 w-4" />
                  Filters
                </Button>
              </div>
              <p className="text-xs text-ink/50">
                Catalog browse across connected stores — shelves stay personalized.
              </p>
            </div>
          ) : null}
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
        <main className={cn("flex flex-col gap-8 px-4 py-6 sm:px-5", pageWidthClass)}>
          {panel === "results" ? (
            <div className="flex flex-col gap-6 lg:flex-row lg:items-start">
              <div
                className={cn(
                  // Header sits outside this scrollport (~13–15rem); keep the
                  // rail inside the visible area so overflow-y can scroll it.
                  // Keep alerts out of this column so they cannot shrink the
                  // sticky height and clip the bottom of the filter rail.
                  "min-h-0 lg:sticky lg:top-3 lg:block lg:h-[calc(100dvh-15rem)] lg:max-h-[calc(100dvh-15rem)] lg:self-start lg:w-56 xl:w-64",
                  filtersOpen ? "block max-h-[min(70vh,32rem)]" : "hidden lg:block",
                )}
              >
                <DiscoverFilterRail
                  authorOptions={facetOptions.authors}
                  narratorOptions={facetOptions.narrators}
                  seriesOptions={facetOptions.series}
                  genreOptions={facetOptions.genres}
                  sourceOptions={facetOptions.sources}
                  filterAuthors={filterAuthors}
                  filterNarrators={filterNarrators}
                  filterSeries={filterSeries}
                  filterGenres={filterGenres}
                  excludedSources={excludedSources}
                  minRating={minRating}
                  runtimeBucket={runtimeBucket}
                  onAuthorsChange={(vals) => {
                    setFilterAuthors(vals);
                    refetchWithFilters({ authors: vals });
                  }}
                  onNarratorsChange={(vals) => {
                    setFilterNarrators(vals);
                    refetchWithFilters({ narrators: vals });
                  }}
                  onSeriesChange={(vals) => {
                    setFilterSeries(vals);
                    refetchWithFilters({ series: vals });
                  }}
                  onGenresChange={(vals) => {
                    setFilterGenres(vals);
                    refetchWithFilters({ genres: vals });
                  }}
                  onExcludedSourcesChange={(vals) => {
                    setExcludedSources(vals);
                    refetchWithFilters({ excludeSources: vals });
                  }}
                  onMinRatingChange={(val) => {
                    setMinRating(val);
                    refetchWithFilters({ minRating: val });
                  }}
                  onRuntimeBucketChange={(val) => {
                    setRuntimeBucket(val);
                    refetchWithFilters({ runtimeBucket: val });
                  }}
                />
              </div>

              <div className="min-w-0 flex-1 space-y-4">
                {error ? (
                  <p
                    className="rounded-md border border-brick/25 bg-brick/5 px-3 py-2 text-sm text-brick"
                    role="alert"
                  >
                    {error}
                  </p>
                ) : null}
                <div className="space-y-1">
                  <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">
                    Results for “{resultsQ}”
                  </h1>
                  <p className="flex items-center gap-2 text-sm text-ink/60">
                    {resultsBusy ? (
                      <>
                        <WaveformThrobber size="sm" className="shrink-0" />
                        <span>Searching storefront catalogs…</span>
                      </>
                    ) : (
                      <span>
                        {`${resultTitles.length} title${resultTitles.length === 1 ? "" : "s"} · click for details`}
                        {resultsHasMore ? " · scroll for more" : ""}
                      </span>
                    )}
                  </p>
                </div>

                {resultsBusy && resultTitles.length === 0 ? (
                  <div
                    className="flex flex-col items-center justify-center gap-4 border border-dashed border-ink/15 bg-white/25 px-4 py-16 text-center"
                    role="status"
                    aria-live="polite"
                    aria-busy="true"
                  >
                    <WaveformThrobber size="lg" />
                    <div className="space-y-1">
                      <p className="text-sm font-medium text-ink/70">
                        Searching storefronts
                      </p>
                      <p className="text-xs text-ink/45">
                        Audible, Libro.fm, Chirp, and GraphicAudio — usually a few seconds
                      </p>
                    </div>
                  </div>
                ) : resultTitles.length === 0 ? (
                  <p className="text-sm text-ink/50">
                    No catalog matches for this view.
                  </p>
                ) : (
                  <>
                    <ul className="divide-y divide-ink/10 border border-ink/10 bg-white/35">
                      {resultTitles.map((title) => (
                        <ResultRow
                          key={title.work_key}
                          title={title}
                          busy={busy}
                          onOpen={() => setSelected(title)}
                          onWishlist={() => void onWishlistTitle(title)}
                        />
                      ))}
                    </ul>
                    {resultsHasMore ? (
                      <div
                        ref={resultsSentinelRef}
                        className="flex items-center justify-center gap-2 py-4 text-sm text-ink/50"
                      >
                        {resultsLoadingMore ? (
                          <>
                            <Loader2
                              className="h-4 w-4 animate-spin text-teal"
                              aria-hidden
                            />
                            <span>Loading more…</span>
                          </>
                        ) : (
                          <span>Scroll for more results</span>
                        )}
                      </div>
                    ) : null}
                  </>
                )}
              </div>
            </div>
          ) : (
            <>
              {error ? (
                <p
                  className="rounded-md border border-brick/25 bg-brick/5 px-3 py-2 text-sm text-brick"
                  role="alert"
                >
                  {error}
                </p>
              ) : null}
              <div className="space-y-1">
                <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">
                  Discover
                </h1>
                <p className="text-sm text-ink/60">
                  Personalized shelves from your library, listening, and storefront catalogs.
                </p>
              </div>

              {feed.shelves.length === 0 ? (
                <p className="text-sm text-ink/50">
                  No recommendations yet — finish or rate a few titles.
                </p>
              ) : (
                <>
                  {shownShelves.map((shelf) => (
                    <ShelfSection
                      key={shelf.id}
                      shelf={shelf}
                      wishlist={wishlist}
                      busy={busy}
                      onOpen={(title) => setSelected(title)}
                      onWishlist={(title) => void onWishlistTitle(title)
                      }
                    />
                  ))}
                  {shownShelves.length < feed.shelves.length ? (
                    <div ref={shelfSentinelRef} className="h-6" aria-hidden />
                  ) : null}
                </>
              )}
            </>
          )}
        </main>
      </div>

      {selected ? (
        <TitleDetailModal
          key={selected.work_key || selected.title}
          detail={titleDetailFromCatalog(selected, {
            purchase_hints: selected.purchase_hints,
          })}
          busy={busy}
          onClose={() => setSelected(null)}
          onWishlist={(t) => void onWishlistTitle(t)}
          onRemoveWishlist={(t) => void onRemoveWishlist(t)}
          onMetaSearch={(kind: TitleMetaSearchKind, value: string) => {
            setSelected(null);
            const composed = discoverSearchFromMeta(kind, value);
            void runResultsSearch(composed.q, composed);
          }}
        />
      ) : null}
    </div>
  );
}

const RESULT_GENRE_CAP_MOBILE = 1;
const RESULT_GENRE_CAP = 3;

function ResultRow({
  title,
  busy,
  onOpen,
  onWishlist,
}: {
  title: CatalogTitle;
  busy: boolean;
  onOpen: () => void;
  onWishlist: () => void;
}) {
  const wishlisted = Boolean(title.wishlist_uuid);
  const authors = title.authors?.trim() || "Unknown author";
  const secondaryFacts = [
    title.narrators ? `narr. ${title.narrators}` : null,
    formatSeriesLabel(title.series, title.series_index),
    title.length_minutes ? formatDuration(title.length_minutes) : null,
    title.publisher,
  ].filter(Boolean) as string[];
  const genres = splitMetaList(title.genres);
  const shownGenres = genres.slice(0, RESULT_GENRE_CAP);
  const extraGenres = genres.length - shownGenres.length;
  const sources = title.sources.length
    ? title.sources
    : (title.store_editions ?? []).map((e) => e.source);
  const uniqueSources = [...new Set(sources.map((s) => s.trim()).filter(Boolean))];

  return (
    <li className="grid grid-cols-[56px_1fr_auto] items-start gap-3 px-3 py-2.5 transition-colors hover:bg-white/50 sm:grid-cols-[64px_1fr_auto] sm:gap-4 sm:px-4">
      <button
        type="button"
        className="overflow-hidden rounded-sm"
        onClick={onOpen}
        aria-label={`Open details for ${title.title}`}
      >
        <CoverThumb url={title.cover_url} className="h-14 w-14 sm:h-16 sm:w-16" />
      </button>
      <button type="button" className="min-w-0 text-left" onClick={onOpen}>
        <h2 className="line-clamp-2 font-display text-base font-semibold leading-tight text-ink hover:underline sm:line-clamp-1 sm:text-lg">
          {title.title}
        </h2>
        <p className="mt-0.5 line-clamp-2 text-xs text-ink/70 sm:line-clamp-1 sm:text-sm">
          {authors}
        </p>
        {secondaryFacts.length > 0 ? (
          <p className="mt-0.5 line-clamp-2 text-xs leading-snug text-ink/55 sm:line-clamp-1 sm:text-sm">
            {secondaryFacts.join(" · ")}
          </p>
        ) : null}
        {uniqueSources.length > 0 || shownGenres.length > 0 ? (
          <div className="mt-1.5 flex items-center gap-1.5 overflow-hidden">
            {uniqueSources.map((source) => (
              <StoreLogo
                key={source}
                source={source}
                className="h-4 w-4 shrink-0"
              />
            ))}
            {shownGenres.map((genre, i) => (
              <span
                key={genre}
                className={cn(
                  "inline-flex max-w-[9rem] shrink-0 items-center truncate rounded-md bg-ink/8 px-2 py-0.5 text-[11px] font-medium normal-case tracking-normal text-ink/75 sm:max-w-none sm:text-xs",
                  i >= RESULT_GENRE_CAP_MOBILE && "hidden sm:inline-flex",
                )}
              >
                {genre}
              </span>
            ))}
            {extraGenres > 0 ? (
              <span className="hidden shrink-0 text-[11px] font-medium text-ink/45 sm:inline">
                +{extraGenres}
              </span>
            ) : null}
          </div>
        ) : null}
      </button>
      <Button
        variant={wishlisted ? "secondary" : "ghost"}
        className="h-9 shrink-0 gap-1 px-2 text-[11px]"
        disabled={busy || wishlisted}
        onClick={onWishlist}
        aria-label={wishlisted ? "Wishlisted" : "Add to wishlist"}
      >
        <Bookmark className={`h-3.5 w-3.5 ${wishlisted ? "fill-current" : ""}`} />
        <span className="hidden sm:inline">
          {wishlisted ? "Wishlisted" : "Wishlist"}
        </span>
      </Button>
    </li>
  );
}

function ShelfSection({
  shelf,
  wishlist,
  busy,
  onOpen,
  onWishlist,
}: {
  shelf: DiscoverShelf;
  wishlist: TitleRequest[];
  busy: boolean;
  onOpen: (title: CatalogTitle) => void;
  onWishlist: (title: CatalogTitle) => void;
}) {
  const [visible, setVisible] = useState(SHELF_CHUNK);
  const scrollerRef = useRef<HTMLDivElement | null>(null);

  const orderedItems = useMemo(
    () => preferCatalogLanguageOrder(shelf.items),
    [shelf.items],
  );

  useEffect(() => {
    setVisible(SHELF_CHUNK);
  }, [shelf.id, orderedItems.length]);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const onScroll = () => {
      if (el.scrollLeft + el.clientWidth >= el.scrollWidth - 96) {
        setVisible((n) => Math.min(n + SHELF_CHUNK, orderedItems.length));
      }
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, [orderedItems.length]);

  const items = orderedItems.slice(0, visible);

  return (
    <section className="space-y-3">
      <div>
        <h2 className="text-lg font-semibold text-ink">{shelf.title}</h2>
        {shelf.subtitle ? (
          <p className="text-sm text-ink/55">{shelf.subtitle}</p>
        ) : null}
      </div>
      <div
        ref={scrollerRef}
        className="-mx-4 flex gap-3 overflow-x-auto px-4 pb-2 snap-x"
      >
        {items.map((r, i) => (
          <ShelfCard
            key={`${shelf.id}-${r.asin ?? r.isbn ?? r.title}-${i}`}
            rec={r}
            wishlistUuid={wishlistUuidForRec(wishlist, r)}
            wishlisted={wishlist.some((req) => wishMatchesRec(req, r))}
            busy={busy}
            onOpen={onOpen}
            onWishlist={onWishlist}
          />
        ))}
        {visible < orderedItems.length ? (
          <div className="flex w-8 shrink-0 items-center justify-center text-xs text-ink/35">
            …
          </div>
        ) : null}
      </div>
    </section>
  );
}

function ShelfCard({
  rec,
  wishlistUuid,
  wishlisted,
  busy,
  onOpen,
  onWishlist,
}: {
  rec: Recommendation;
  wishlistUuid: string | null;
  wishlisted: boolean;
  busy: boolean;
  onOpen: (title: CatalogTitle) => void;
  onWishlist: (title: CatalogTitle) => void;
}) {
  const cardRef = useRef<HTMLElement | null>(null);
  const [inView, setInView] = useState(false);
  const [title, setTitle] = useState(() =>
    catalogTitleFromRec(rec, wishlistUuid),
  );
  const [, setHints] = useState<PurchaseHint[]>(rec.purchase_hints);
  const [best, setBest] = useState<PurchaseHint | null>(
    rec.purchase_hints[0] ?? null,
  );
  const [pricing, setPricing] = useState<"idle" | "loading" | "done">("idle");

  useEffect(() => {
    setTitle(catalogTitleFromRec(rec, wishlistUuid));
  }, [rec, wishlistUuid]);

  useEffect(() => {
    const el = cardRef.current;
    if (!el) return;
    const obs = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setInView(true);
          obs.disconnect();
        }
      },
      { rootMargin: "120px" },
    );
    obs.observe(el);
    return () => obs.disconnect();
  }, []);

  useEffect(() => {
    if (!inView) return;
    let cancelled = false;
    setHints(rec.purchase_hints);
    setBest(rec.purchase_hints[0] ?? null);
    setPricing("loading");
    void (async () => {
      try {
        const [res, meta] = await Promise.all([
          fetchPurchaseHints({
            title: rec.title,
            authors: rec.authors,
            asin: rec.asin,
            isbn: rec.isbn,
            candidate_source: rec.candidate_source,
            candidate_product_id: rec.candidate_product_id,
            store_editions: rec.store_editions,
          }),
          titleNeedsShelfMeta(catalogTitleFromRec(rec, wishlistUuid))
            ? fetchTitleMeta({
                title: rec.title,
                authors: rec.authors,
                asin: rec.asin,
                isbn: rec.isbn,
                narrators: rec.narrators,
                length_minutes: rec.length_minutes,
              })
            : Promise.resolve(null),
        ]);
        if (cancelled) return;
        setHints(res.hints);
        setBest(res.best);
        if (meta) {
          setTitle((prev) =>
            applyTitleMeta(
              { ...prev, purchase_hints: res.hints },
              meta,
            ),
          );
        } else {
          setTitle((prev) => ({ ...prev, purchase_hints: res.hints }));
        }
      } catch {
        // Keep seed links / sparse catalog fields from the feed.
      } finally {
        if (!cancelled) setPricing("done");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [
    inView,
    rec,
    wishlistUuid,
  ]);

  const blurb = descriptionPlainText(title.description);
  const seriesLabel = formatSeriesLabel(title.series, title.series_index);
  const bestPrice = best ? formatShelfBestPrice(best) : null;

  return (
    <article
      ref={cardRef}
      className="w-40 shrink-0 snap-start rounded-lg bg-white/50 p-2.5 shadow-sm ring-1 ring-ink/5 sm:w-44"
    >
      <button
        type="button"
        className="w-full text-left"
        onClick={() => onOpen(title)}
      >
        <CoverThumb
          url={title.cover_url}
          className="aspect-square w-full rounded-md shadow-sm"
        />
        <p className="mt-2.5 line-clamp-2 text-sm font-semibold leading-snug text-ink hover:underline">
          {title.title}
        </p>
        <p className="mt-1 line-clamp-1 text-xs text-ink/55">
          {title.authors ?? "Unknown author"}
        </p>
        {seriesLabel || title.length_minutes ? (
          <p className="mt-0.5 line-clamp-1 text-[11px] text-ink/45">
            {[
              seriesLabel,
              title.length_minutes
                ? formatDuration(title.length_minutes)
                : null,
            ]
              .filter(Boolean)
              .join(" · ")}
          </p>
        ) : null}
        {!title.cover_url && (blurb || rec.reasons[0]) ? (
          <p className="mt-1.5 line-clamp-2 text-[11px] leading-snug text-ink/45">
            {blurb || rec.reasons[0]}
          </p>
        ) : null}
      </button>

      {title.rating_overall != null ? (
        <div className="mt-1.5 flex items-center gap-1">
          <StarRating
            value={title.rating_overall}
            starClassName="h-3 w-3"
          />
          <span className="text-[11px] tabular-nums text-ink/55">
            {title.rating_overall.toFixed(1)}
          </span>
        </div>
      ) : null}

      {best || pricing === "loading" ? (
        <div className={title.rating_overall != null ? "mt-1" : "mt-2"}>
          {best?.url ? (
            <a
              href={best.url}
              target="_blank"
              rel="noreferrer"
              className="inline-flex max-w-full items-center gap-1.5 text-[12px] font-semibold text-teal hover:underline"
              onClick={(e) => e.stopPropagation()}
              aria-label={`Best price at ${best.source}${bestPrice ? `: ${bestPrice}` : ""}`}
            >
              <StoreLogo source={best.source} className="h-4 w-4" />
              <span className="tabular-nums">
                {bestPrice ?? (pricing === "loading" ? "…" : "Open")}
              </span>
            </a>
          ) : (
            <p className="inline-flex max-w-full items-center gap-1.5 text-[12px] font-semibold text-ink/70">
              {best ? <StoreLogo source={best.source} className="h-4 w-4" /> : null}
              <span className="tabular-nums">
                {bestPrice ?? (pricing === "loading" ? "…" : "")}
              </span>
            </p>
          )}
        </div>
      ) : null}

      <div className="mt-2.5">
        <Button
          variant={wishlisted ? "secondary" : "ghost"}
          className="h-8 w-full justify-center gap-1.5 px-2 text-[11px]"
          disabled={busy || wishlisted}
          onClick={(e) => {
            e.stopPropagation();
            onWishlist(title);
          }}
          aria-label={wishlisted ? "Already on your wishlist" : "Wishlist this title"}
        >
          <Bookmark className={`h-3.5 w-3.5 ${wishlisted ? "fill-current" : ""}`} />
          {wishlisted ? "Wishlisted" : "Wishlist"}
        </Button>
      </div>
    </article>
  );
}
