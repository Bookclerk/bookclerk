import type { AppView } from "@/lib/api";

/** Document paths the React app owns (served as `index.html` by bookclerkd). */
const APP_PATHS = new Set([
  "/",
  "/discover",
  "/library",
  "/wishlist",
  "/accounts",
  "/settings",
]);

/** True when `pathname` is a known GUI route (not an API/static/unknown URL). */
export function isAppPath(pathname: string): boolean {
  return APP_PATHS.has(normalizePathname(pathname));
}

/** Strip a trailing slash (except for `/`). */
export function normalizePathname(pathname: string): string {
  const raw = pathname || "/";
  if (raw.length > 1 && raw.endsWith("/")) {
    return raw.slice(0, -1);
  }
  return raw === "" ? "/" : raw;
}

/** Map a document path to an app view, or `null` for `/` (use default_view). */
export function viewFromPath(pathname: string): AppView | null {
  switch (normalizePathname(pathname)) {
    case "/discover":
      return "discover";
    case "/library":
      return "library";
    case "/wishlist":
      return "wishlist";
    case "/accounts":
      return "accounts";
    case "/settings":
      return "settings";
    default:
      return null;
  }
}

/** Canonical document path for an app view. */
export function pathForView(view: AppView): string {
  switch (view) {
    case "discover":
      return "/discover";
    case "library":
      return "/library";
    case "wishlist":
      return "/wishlist";
    case "accounts":
      return "/accounts";
    case "settings":
      return "/settings";
  }
}

/** Resolve the view to show for the current URL + signed-in default. */
export function resolveView(
  pathname: string,
  defaultView: AppView | string | undefined,
): AppView {
  return viewFromPath(pathname) ?? normalizeAppView(defaultView);
}

export function normalizeAppView(v: string | undefined): AppView {
  if (
    v === "library" ||
    v === "accounts" ||
    v === "discover" ||
    v === "wishlist" ||
    v === "settings"
  ) {
    return v;
  }
  return "discover";
}

/** Update the URL bar without a full reload. */
export function syncUrlToView(view: AppView, mode: "push" | "replace" = "push") {
  const next = pathForView(view);
  const current = normalizePathname(window.location.pathname);
  if (current === next) return;
  const url = `${next}${window.location.search}${window.location.hash}`;
  if (mode === "replace") {
    window.history.replaceState(null, "", url);
  } else {
    window.history.pushState(null, "", url);
  }
}

const DISCOVER_SEARCH_HANDOFF = "bookclerk.discoverSearch";

/** Optional post-search filter/sort applied after a Discover catalog query. */
export type DiscoverSearchHandoff = {
  q: string;
  /** Storefront-scoped search (`author` / `narrator` / `series` / `genre`). */
  field?: "author" | "narrator" | "series" | "genre";
  sort?:
    | "relevance"
    | "popularity"
    | "rating"
    | "title"
    | "author"
    | "price"
    | "length";
  sortDir?: "asc" | "desc";
  /** Seed include filters (server-applied). */
  authors?: string[];
  narrators?: string[];
  series?: string[];
  genres?: string[];
  sources?: string[];
  excludeSources?: string[];
  minRating?: number;
  runtimeBucket?: "any" | "under6" | "6to12" | "12to20" | "over20";
  /**
   * Hard language include (`en`, `zh`, …) or `__all__` for no hard filter.
   * Omit to use the browser language default.
   */
  language?: string;
  /** When false, do not exclude Virtual Voice (default hide is on). */
  hideVirtualVoice?: boolean;
  /** @deprecated legacy single-dimension filter */
  filterKind?:
    | "all"
    | "authors"
    | "series"
    | "narrators"
    | "genres"
    | "sources";
  /** @deprecated */
  filterValue?: string;
  /** @deprecated */
  sortKey?:
    | "title"
    | "author"
    | "series"
    | "relevance"
    | "popularity"
    | "rating"
    | "price"
    | "length";
};

/** Queue a Discover catalog search for the next Discover page mount (cross-view). */
export function queueDiscoverSearch(handoff: string | DiscoverSearchHandoff) {
  const payload: DiscoverSearchHandoff =
    typeof handoff === "string" ? { q: handoff } : { ...handoff };
  payload.q = payload.q.trim();
  if (payload.q.length < 2) return;
  try {
    sessionStorage.setItem(DISCOVER_SEARCH_HANDOFF, JSON.stringify(payload));
  } catch {
    // private mode / quota — Discover simply won't auto-search
  }
}

/** Consume a queued Discover search (once). */
export function takeQueuedDiscoverSearch(): DiscoverSearchHandoff | null {
  try {
    const raw = sessionStorage.getItem(DISCOVER_SEARCH_HANDOFF);
    if (!raw) return null;
    sessionStorage.removeItem(DISCOVER_SEARCH_HANDOFF);
    // Back-compat: plain string query from older builds / callers.
    if (raw.startsWith("{")) {
      const parsed = JSON.parse(raw) as DiscoverSearchHandoff;
      const q = typeof parsed.q === "string" ? parsed.q.trim() : "";
      if (q.length < 2) return null;
      return { ...parsed, q };
    }
    const q = raw.trim();
    return q.length >= 2 ? { q } : null;
  } catch {
    return null;
  }
}
