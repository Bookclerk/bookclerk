import { useEffect, useRef, useState } from "react";
import { Bookmark, LogOut, Search, Settings2, Sparkles } from "lucide-react";
import { AppNav, type AppNavProps } from "@/components/AppNav";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  createWishlistItem,
  fetchDiscoverFeed,
  fetchPreferences,
  fetchPurchaseHints,
  fetchWishlist,
  patchPreferences,
  searchCatalog,
  signOut,
  type AppView,
  type AuthRole,
  type CatalogSearchHit,
  type DiscoverFeed,
  type DiscoverShelf,
  type PurchaseHint,
  type Recommendation,
  type ShelfKindInfo,
  type TitleRequest,
} from "@/lib/api";

const SHELF_CHUNK = 8;
const SHELVES_INITIAL = 6;

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

export function DiscoverPage({
  onLogout,
  nav,
  role,
  defaultView,
  onDefaultViewChange,
}: {
  onLogout: () => void;
  nav: AppNavProps;
  role?: AuthRole;
  defaultView: AppView;
  onDefaultViewChange?: (view: AppView) => void;
}) {
  const [feed, setFeed] = useState<DiscoverFeed>({ shelves: [] });
  const [wishlist, setWishlist] = useState<TitleRequest[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [ignored, setIgnored] = useState<string[]>([]);
  const [prefsView, setPrefsView] = useState<AppView>(defaultView);
  const [prefsOpen, setPrefsOpen] = useState(false);
  const [prefsReady, setPrefsReady] = useState(false);
  const [visibleShelves, setVisibleShelvesCount] = useState(SHELVES_INITIAL);
  const shelfSentinelRef = useRef<HTMLDivElement | null>(null);

  const [searchQ, setSearchQ] = useState("");
  const [suggestions, setSuggestions] = useState<CatalogSearchHit[]>([]);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchBusy, setSearchBusy] = useState(false);
  const searchWrapRef = useRef<HTMLDivElement | null>(null);
  const searchSeq = useRef(0);

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
      setError(err instanceof Error ? err.message : "Failed to load discovery");
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      setBusy(true);
      setError(null);
      try {
        const prefs = await fetchPreferences();
        if (cancelled) return;
        setIgnored(prefs.disabled_shelves);
        setPrefsView(prefs.default_view);
        await refreshFeed();
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : "Failed to load discovery");
        }
      } finally {
        if (!cancelled) {
          setPrefsReady(true);
          setBusy(false);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const q = searchQ.trim();
    if (q.length < 2) {
      setSuggestions([]);
      setSearchBusy(false);
      return;
    }
    const seq = ++searchSeq.current;
    setSearchBusy(true);
    const t = window.setTimeout(() => {
      void (async () => {
        try {
          const hits = await searchCatalog(q, 10);
          if (seq !== searchSeq.current) return;
          setSuggestions(hits);
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
  }, [searchQ]);

  useEffect(() => {
    function onDocClick(e: MouseEvent) {
      if (!searchWrapRef.current?.contains(e.target as Node)) {
        setSearchOpen(false);
      }
    }
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, []);

  async function toggleIgnored(kindId: string) {
    const prev = ignored;
    const next = prev.includes(kindId)
      ? prev.filter((x) => x !== kindId)
      : [...prev, kindId];
    setIgnored(next);
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences({ disabled_shelves: next });
      setIgnored(saved.disabled_shelves);
      await refreshFeed();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save shelf prefs");
      setIgnored(prev);
    } finally {
      setBusy(false);
    }
  }

  async function onDefaultViewSelect(view: AppView) {
    const prev = prefsView;
    setPrefsView(view);
    setBusy(true);
    setError(null);
    try {
      const saved = await patchPreferences({ default_view: view });
      setPrefsView(saved.default_view);
      onDefaultViewChange?.(saved.default_view);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to save default view");
      setPrefsView(prev);
    } finally {
      setBusy(false);
    }
  }

  async function onWishlist(rec: Recommendation) {
    if (wishlist.some((r) => wishMatchesRec(r, rec))) return;
    setBusy(true);
    setError(null);
    try {
      await createWishlistItem({
        title: rec.title,
        authors: rec.authors ?? undefined,
        asin: rec.asin ?? undefined,
        isbn: rec.isbn ?? undefined,
        work_key: rec.work_key || undefined,
        store_editions: rec.store_editions,
        notes: "Wishlisted from Discover",
      });
      await refreshFeed();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to wishlist title");
    } finally {
      setBusy(false);
    }
  }

  async function onWishlistHit(hit: CatalogSearchHit) {
    if (wishlist.some((r) => wishMatchesHit(r, hit))) return;
    setBusy(true);
    setError(null);
    try {
      await createWishlistItem({
        title: hit.title,
        authors: hit.authors ?? undefined,
        asin: hit.asin ?? undefined,
        isbn: hit.isbn ?? undefined,
        work_key: hit.work_key,
        store_editions: hit.store_editions,
        notes: "Wishlisted from catalog search",
      });
      setSearchOpen(false);
      setSearchQ("");
      setSuggestions([]);
      await refreshFeed();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to wishlist title");
    } finally {
      setBusy(false);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  const shelfKinds: ShelfKindInfo[] = feed.shelf_kinds?.length
    ? feed.shelf_kinds
    : [];
  // Server already applied disabled_shelves; do not re-filter client-side.
  const shownShelves = feed.shelves.slice(0, visibleShelves);

  useEffect(() => {
    const el = shelfSentinelRef.current;
    if (!el) return;
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
  }, [feed.shelves.length, shownShelves.length]);

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className="mx-auto flex max-w-6xl flex-col gap-3">
          <div className="flex items-center justify-between gap-3">
            <div className="flex items-center gap-3 sm:gap-5">
              <img
                src="/bookclerk-logo.svg"
                alt="Bookclerk"
                className="h-8 w-auto sm:h-9"
              />
              <AppNav {...nav} />
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="ghost"
                onClick={() => setPrefsOpen((o) => !o)}
                aria-label="Preferences"
                aria-expanded={prefsOpen}
              >
                <Settings2 className="h-4 w-4" />
              </Button>
              <Button
                variant="secondary"
                onClick={() => void refresh()}
                disabled={busy}
              >
                <Sparkles className="h-4 w-4" />
                Refresh
              </Button>
              <Button variant="ghost" onClick={() => void onSignOut()} aria-label="Sign out">
                <LogOut className="h-4 w-4" />
              </Button>
            </div>
          </div>

          <div ref={searchWrapRef} className="relative">
            <div className="relative">
              <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-ink/40" />
              <Input
                value={searchQ}
                onChange={(e) => {
                  setSearchQ(e.target.value);
                  setSearchOpen(true);
                }}
                onFocus={() => {
                  if (suggestions.length > 0) setSearchOpen(true);
                }}
                placeholder="Search Audible, Libro.fm, Chirp, GraphicAudio…"
                className="h-11 pl-9"
                aria-label="Search store catalogs"
                aria-autocomplete="list"
                aria-expanded={searchOpen}
              />
            </div>
            {searchOpen && (searchBusy || suggestions.length > 0 || searchQ.trim().length >= 2) ? (
              <ul
                className="absolute z-20 mt-1 max-h-80 w-full overflow-auto border border-ink/10 bg-paper shadow-lg"
                role="listbox"
              >
                {searchBusy && suggestions.length === 0 ? (
                  <li className="px-3 py-2 text-sm text-ink/50">Searching catalogs…</li>
                ) : null}
                {!searchBusy && suggestions.length === 0 && searchQ.trim().length >= 2 ? (
                  <li className="px-3 py-2 text-sm text-ink/50">No catalog matches.</li>
                ) : null}
                {suggestions.map((hit) => {
                  const already = wishlist.some((r) => wishMatchesHit(r, hit));
                  return (
                    <li
                      key={hit.work_key}
                      role="option"
                      className="flex items-center justify-between gap-2 border-b border-ink/5 px-3 py-2 last:border-0"
                    >
                      <div className="min-w-0">
                        <p className="truncate text-sm font-medium text-ink">{hit.title}</p>
                        <p className="truncate text-xs text-ink/50">
                          {hit.authors ?? "Unknown author"}
                          {hit.sources.length
                            ? ` · ${hit.sources.map(storeLabel).join(", ")}`
                            : ""}
                        </p>
                      </div>
                      <Button
                        variant={already ? "secondary" : "ghost"}
                        className="h-8 shrink-0 gap-1 px-2 text-[11px]"
                        disabled={busy || already}
                        onClick={() => void onWishlistHit(hit)}
                      >
                        <Bookmark className={`h-3.5 w-3.5 ${already ? "fill-current" : ""}`} />
                        {already ? "Wishlisted" : "Wishlist"}
                      </Button>
                    </li>
                  );
                })}
              </ul>
            ) : null}
          </div>
        </div>
      </header>

      <main className="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-10 overflow-auto px-4 py-6">
        {error ? (
          <p className="text-sm font-medium text-brick" role="alert">
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

        {prefsOpen && prefsReady ? (
          <section className="space-y-5 border border-ink/10 bg-white/40 p-4">
            <div className="space-y-2">
              <h2 className="text-base font-semibold text-ink">Default view</h2>
              <p className="text-sm text-ink/55">
                Where this account opens after sign-in. Saved to your Bookclerk profile.
              </p>
              <div className="flex flex-wrap gap-2">
                {(
                  [
                    ["discover", "Discover"],
                    ["wishlist", "Wishlist"],
                    ["library", "Library"],
                    ["accounts", "Accounts"],
                  ] as const
                ).map(([id, label]) => (
                  <Button
                    key={id}
                    variant={prefsView === id ? "secondary" : "ghost"}
                    disabled={busy}
                    onClick={() => void onDefaultViewSelect(id)}
                  >
                    {label}
                  </Button>
                ))}
              </div>
            </div>

            {shelfKinds.length > 0 ? (
              <div className="space-y-3">
                <div>
                  <h2 className="text-base font-semibold text-ink">Shelves to show</h2>
                  <p className="text-sm text-ink/55">
                    All shelves are on by default. Uncheck any you want to hide for this
                    account.
                  </p>
                </div>
                <ul className="grid gap-2 sm:grid-cols-2">
                  {shelfKinds.map((kind) => {
                    const on = !ignored.includes(kind.id);
                    return (
                      <li key={kind.id}>
                        <label className="flex cursor-pointer items-start gap-2 text-sm text-ink">
                          <input
                            type="checkbox"
                            className="mt-0.5"
                            checked={on}
                            disabled={busy}
                            onChange={() => void toggleIgnored(kind.id)}
                          />
                          <span>
                            <span className="font-medium">{kind.label}</span>
                            <span className="block text-xs text-ink/45">{kind.id}</span>
                          </span>
                        </label>
                      </li>
                    );
                  })}
                </ul>
              </div>
            ) : null}
          </section>
        ) : null}

        {feed.shelves.length === 0 ? (
          <p className="text-sm text-ink/50">No recommendations yet — finish or rate a few titles.</p>
        ) : (
          <>
            {shownShelves.map((shelf) => (
              <ShelfSection
                key={shelf.id}
                shelf={shelf}
                wishlist={wishlist}
                busy={busy}
                onWishlist={onWishlist}
              />
            ))}
            {shownShelves.length < feed.shelves.length ? (
              <div ref={shelfSentinelRef} className="h-6" aria-hidden />
            ) : null}
          </>
        )}
      </main>
    </div>
  );
}

function ShelfSection({
  shelf,
  wishlist,
  busy,
  onWishlist,
}: {
  shelf: DiscoverShelf;
  wishlist: TitleRequest[];
  busy: boolean;
  onWishlist: (rec: Recommendation) => void;
}) {
  const [visible, setVisible] = useState(SHELF_CHUNK);
  const scrollerRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    setVisible(SHELF_CHUNK);
  }, [shelf.id, shelf.items.length]);

  useEffect(() => {
    const el = scrollerRef.current;
    if (!el) return;
    const onScroll = () => {
      if (el.scrollLeft + el.clientWidth >= el.scrollWidth - 96) {
        setVisible((n) => Math.min(n + SHELF_CHUNK, shelf.items.length));
      }
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    return () => el.removeEventListener("scroll", onScroll);
  }, [shelf.items.length]);

  const items = shelf.items.slice(0, visible);

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
            wishlisted={wishlist.some((req) => wishMatchesRec(req, r))}
            busy={busy}
            onWishlist={() => onWishlist(r)}
          />
        ))}
        {visible < shelf.items.length ? (
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
  wishlisted,
  busy,
  onWishlist,
}: {
  rec: Recommendation;
  wishlisted: boolean;
  busy: boolean;
  onWishlist: () => void;
}) {
  const cardRef = useRef<HTMLElement | null>(null);
  const [inView, setInView] = useState(false);
  const [hints, setHints] = useState<PurchaseHint[]>(rec.purchase_hints);
  const [best, setBest] = useState<PurchaseHint | null>(
    rec.purchase_hints[0] ?? null,
  );
  const [pricing, setPricing] = useState<"idle" | "loading" | "done">("idle");

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
        const res = await fetchPurchaseHints({
          title: rec.title,
          authors: rec.authors,
          asin: rec.asin,
          isbn: rec.isbn,
          candidate_source: rec.candidate_source,
          candidate_product_id: rec.candidate_product_id,
          store_editions: rec.store_editions,
        });
        if (cancelled) return;
        setHints(res.hints);
        setBest(res.best);
      } catch {
        // Keep seed links from the feed.
      } finally {
        if (!cancelled) setPricing("done");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [
    inView,
    rec.title,
    rec.authors,
    rec.asin,
    rec.isbn,
    rec.candidate_source,
    rec.candidate_product_id,
  ]);

  const others = hints.filter(
    (h) =>
      !(
        best &&
        h.source === best.source &&
        h.product_id === best.product_id
      ),
  );

  return (
    <article
      ref={cardRef}
      className="w-56 shrink-0 snap-start rounded-lg bg-white/50 p-3 shadow-sm ring-1 ring-ink/5"
    >
      <p className="line-clamp-2 text-sm font-medium text-ink">{rec.title}</p>
      <p className="mt-1 line-clamp-1 text-xs text-ink/55">
        {rec.authors ?? "Unknown author"}
        {rec.series ? ` · ${rec.series}` : ""}
      </p>
      {rec.reasons[0] ? (
        <p className="mt-2 line-clamp-2 text-[11px] leading-snug text-ink/45">{rec.reasons[0]}</p>
      ) : null}

      {best ? (
        <div className="mt-2 space-y-1">
          {best.url ? (
            <a
              href={best.url}
              target="_blank"
              rel="noreferrer"
              className="inline-flex max-w-full items-baseline gap-1 text-[12px] font-semibold text-teal underline"
            >
              <span className="truncate capitalize">{storeLabel(best.source)}</span>
              <span className="shrink-0 tabular-nums">
                {best.price_label ?? (pricing === "loading" ? "…" : "")}
              </span>
            </a>
          ) : (
            <p className="text-[12px] font-semibold capitalize text-ink/70">
              {storeLabel(best.source)}
              {best.price_label ? ` · ${best.price_label}` : ""}
            </p>
          )}
          {others.length > 0 ? (
            <p className="flex flex-wrap gap-x-2 gap-y-1 text-[11px] text-ink/55">
              {others.map((h) =>
                h.url ? (
                  <a
                    key={`${h.source}-${h.product_id}`}
                    href={h.url}
                    target="_blank"
                    rel="noreferrer"
                    className="underline hover:text-teal"
                  >
                    {storeLabel(h.source)}
                    {h.price_label ? ` ${h.price_label}` : ""}
                  </a>
                ) : (
                  <span key={`${h.source}-${h.product_id}`}>
                    {storeLabel(h.source)}
                  </span>
                ),
              )}
            </p>
          ) : null}
        </div>
      ) : null}

      <div className="mt-3">
        <Button
          variant={wishlisted ? "secondary" : "ghost"}
          className="h-8 w-full justify-center gap-1.5 px-2 text-[11px]"
          disabled={busy || wishlisted}
          onClick={() => onWishlist()}
          aria-label={wishlisted ? "Already on your wishlist" : "Wishlist this title"}
        >
          <Bookmark className={`h-3.5 w-3.5 ${wishlisted ? "fill-current" : ""}`} />
          {wishlisted ? "Wishlisted" : "Wishlist"}
        </Button>
      </div>
    </article>
  );
}

function storeLabel(source: string): string {
  switch (source.toLowerCase()) {
    case "audible":
      return "Audible";
    case "libro":
      return "Libro.fm";
    case "chirp":
      return "Chirp";
    case "graphicaudio":
      return "GraphicAudio";
    default:
      return source;
  }
}
