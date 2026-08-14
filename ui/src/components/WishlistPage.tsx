import { useEffect, useState } from "react";
import { Trash2 } from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { CoverThumb } from "@/components/CoverThumb";
import {
  TitleDetailModal,
  type TitleMetaSearchKind,
} from "@/components/TitleDetailModal";
import { titleDetailFromCatalog, type TitleDetail } from "@/lib/titleDetail";
import { Button } from "@/components/ui/button";
import {
  applyTitleMeta,
  catalogTitleFromQueueEntry,
  catalogTitleFromRequest,
  discoverSearchFromMeta,
  formatSeriesLabel,
  titleNeedsMeta,
  type CatalogTitle,
} from "@/lib/catalogTitle";
import {
  createWishlistItem,
  fetchRequestQueue,
  fetchTitleMetaBatch,
  fetchWishlist,
  removeWishlistItem,
  signOut,
  type AuthRole,
  type GlobalQueueEntry,
  type TitleRequest,
} from "@/lib/api";
import { cn, pageWidthClass } from "@/lib/utils";
import { formatDuration } from "@/lib/libraryFilters";
import { queueDiscoverSearch } from "@/lib/routes";

/**
 * Wishlist view — personal wishes and the global request queue.
 *
 * @param props - Logout handler, nav props, and optional session role.
 */
export function WishlistPage({
  onLogout,
  nav,
  role,
}: {
  onLogout: () => void;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [mine, setMine] = useState<TitleRequest[]>([]);
  const [mineTitles, setMineTitles] = useState<CatalogTitle[]>([]);
  const [queue, setQueue] = useState<GlobalQueueEntry[]>([]);
  const [queueTitles, setQueueTitles] = useState<CatalogTitle[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState<CatalogTitle | null>(null);

  async function enrichTitles(titles: CatalogTitle[]): Promise<CatalogTitle[]> {
    const sparse = titles
      .map((t, index) => ({ t, index }))
      .filter(({ t }) => titleNeedsMeta(t));
    if (sparse.length === 0) return titles;
    try {
      const metas = await fetchTitleMetaBatch(
        sparse.map(({ t }) => ({
          title: t.title,
          authors: t.authors,
          asin: t.asin,
          isbn: t.isbn,
          narrators: t.narrators,
          length_minutes: t.length_minutes,
        })),
      );
      const next = [...titles];
      sparse.forEach(({ index }, i) => {
        next[index] = applyTitleMeta(next[index]!, metas[i]);
      });
      return next;
    } catch {
      return titles;
    }
  }

  async function refresh() {
    setError(null);
    setBusy(true);
    try {
      const [w, q] = await Promise.all([fetchWishlist(), fetchRequestQueue()]);
      setMine(w);
      setQueue(q);
      const myKeys = new Map(w.map((r) => [r.work_key, r.uuid]));
      const baseMine = w.map((r) => catalogTitleFromRequest(r));
      const baseQueue = q.map((entry) =>
        catalogTitleFromQueueEntry(entry, myKeys.get(entry.work_key) ?? null),
      );
      setMineTitles(baseMine);
      setQueueTitles(baseQueue);
      const [enrichedMine, enrichedQueue] = await Promise.all([
        enrichTitles(baseMine),
        enrichTitles(baseQueue),
      ]);
      setMineTitles(enrichedMine);
      setQueueTitles(enrichedQueue);
      setSelected((prev) => {
        if (!prev) return prev;
        const match =
          enrichedMine.find(
            (r) =>
              r.wishlist_uuid === prev.wishlist_uuid ||
              (r.work_key && r.work_key === prev.work_key),
          ) ??
          enrichedQueue.find((r) => r.work_key && r.work_key === prev.work_key);
        return match ?? { ...prev, wishlist_uuid: null };
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load wishlist");
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function onRemove(uuid: string) {
    setBusy(true);
    setError(null);
    try {
      await removeWishlistItem(uuid);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to remove wishlist item");
      setBusy(false);
    }
  }

  async function onWishlistFromDetail(title: TitleDetail) {
    if (title.wishlist_uuid) return;
    setBusy(true);
    setError(null);
    try {
      const created = await createWishlistItem({
        title: title.title,
        authors: title.authors ?? undefined,
        asin: title.asin ?? undefined,
        isbn: title.isbn ?? undefined,
        work_key: title.work_key || undefined,
        store_editions: title.store_editions ?? undefined,
        purchase_hints: title.purchase_hints ?? undefined,
        cover_url: title.cover_url ?? undefined,
        description: title.description ?? undefined,
        subtitle: title.subtitle ?? undefined,
        narrators: title.narrators ?? undefined,
        series: title.series ?? undefined,
        series_index: title.series_index ?? undefined,
        publisher: title.publisher ?? undefined,
        length_minutes: title.length_minutes ?? undefined,
        published_at: title.published_at ?? undefined,
        genres: title.categories ?? undefined,
        language: title.language ?? undefined,
        notes: "Wishlisted from global queue",
      });
      setSelected((prev) =>
        prev &&
        (prev.work_key === title.work_key || prev.title === title.title)
          ? { ...prev, wishlist_uuid: created.uuid }
          : prev,
      );
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to wishlist title");
      setBusy(false);
    }
  }

  async function onRemoveFromDetail(title: TitleDetail) {
    if (!title.wishlist_uuid) return;
    await onRemove(title.wishlist_uuid);
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  const myKeys = new Set(mine.map((r) => r.work_key).filter(Boolean));

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={pageWidthClass}>
          <AppTopBar nav={nav} onSignOut={onSignOut} />
        </div>
      </header>

      <div className="flex-1 overflow-y-auto px-3 py-5 sm:px-5">
        <main className={cn("grid gap-8 lg:grid-cols-[1.2fr_1fr]", pageWidthClass)}>
          <section className="space-y-4">
            <div className="space-y-1">
              <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">
                Your wishlist
              </h1>
              <p className="text-sm text-ink/60">
                Titles you want acquired. Click a title for details. Un-wishlist to remove
                them from your list and the shared queue.
              </p>
            </div>

            {error ? (
              <p className="text-sm font-medium text-brick" role="alert">
                {error}
              </p>
            ) : null}

            {mineTitles.length === 0 ? (
              <p className="text-sm text-ink/50">
                Nothing yet — wishlist from Discover cards or catalog search.
              </p>
            ) : (
              <ul className="divide-y divide-ink/10 border border-ink/10 bg-white/35">
                {mineTitles.map((title) => (
                  <li
                    key={title.wishlist_uuid ?? title.work_key}
                    className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
                  >
                    <button
                      type="button"
                      className="flex min-w-0 flex-1 items-center gap-3 text-left"
                      onClick={() => setSelected(title)}
                    >
                      <CoverThumb
                        url={title.cover_url}
                        className="h-14 w-14"
                      />
                      <div className="min-w-0">
                        <p className="font-medium text-ink hover:underline">
                          {title.title}
                        </p>
                        <p className="text-xs text-ink/50">
                          {[
                            title.authors ?? "Unknown author",
                            title.narrators ? `narr. ${title.narrators}` : null,
                            formatSeriesLabel(title.series, title.series_index),
                            title.length_minutes
                              ? formatDuration(title.length_minutes)
                              : null,
                            title.asin,
                            title.isbn,
                          ]
                            .filter(Boolean)
                            .join(" · ")}
                        </p>
                      </div>
                    </button>
                    <Button
                      variant="ghost"
                      disabled={busy || !title.wishlist_uuid}
                      onClick={() =>
                        title.wishlist_uuid
                          ? void onRemove(title.wishlist_uuid)
                          : undefined
                      }
                      aria-label="Remove from wishlist"
                    >
                      <Trash2 className="h-4 w-4" />
                      Remove
                    </Button>
                  </li>
                ))}
              </ul>
            )}
          </section>

          <aside className="space-y-3 lg:border-l lg:border-ink/10 lg:pl-6">
            <div className="space-y-1">
              <h2 className="text-base font-semibold text-ink">Global queue</h2>
              <p className="text-xs text-ink/55">
                Shared household ranking: overall Discover taste, heavily boosted by how many
                people wishlisted each title. Titles already in the library are omitted.
              </p>
            </div>
            {queueTitles.length === 0 ? (
              <p className="text-sm text-ink/50">Queue is empty.</p>
            ) : (
              <ol className="space-y-3">
                {queueTitles.map((title, i) => {
                  const entry = queue[i];
                  return (
                    <li key={title.work_key}>
                      <button
                        type="button"
                        className="flex w-full gap-2.5 text-left text-sm"
                        onClick={() => setSelected(title)}
                      >
                        <span className="w-5 shrink-0 pt-1 text-xs tabular-nums text-ink/40">
                          {i + 1}
                        </span>
                        <CoverThumb
                          url={title.cover_url}
                          className="h-12 w-12"
                        />
                        <div className="min-w-0 flex-1">
                          <div className="flex items-baseline justify-between gap-2">
                            <p className="truncate font-medium text-ink hover:underline">
                              {title.title}
                            </p>
                            {entry ? (
                              <span className="shrink-0 tabular-nums text-xs font-semibold text-teal">
                                ×{entry.wish_count}
                              </span>
                            ) : null}
                          </div>
                          <p className="text-xs text-ink/50">
                            {[
                              title.authors ?? "Unknown author",
                              formatSeriesLabel(title.series, title.series_index),
                              title.length_minutes
                                ? formatDuration(title.length_minutes)
                                : null,
                              myKeys.has(title.work_key) ? "on your list" : null,
                            ]
                              .filter(Boolean)
                              .join(" · ")}
                          </p>
                          {title.reasons?.[0] ? (
                            <p className="text-[11px] text-ink/40">{title.reasons[0]}</p>
                          ) : null}
                        </div>
                      </button>
                    </li>
                  );
                })}
              </ol>
            )}
          </aside>
        </main>
      </div>

      {selected ? (
        <TitleDetailModal
          key={selected.work_key || selected.wishlist_uuid || selected.title}
          detail={titleDetailFromCatalog(selected)}
          busy={busy}
          onClose={() => setSelected(null)}
          onWishlist={(t) => void onWishlistFromDetail(t)}
          onRemoveWishlist={(t) => void onRemoveFromDetail(t)}
          onMetaSearch={(kind: TitleMetaSearchKind, value: string) => {
            setSelected(null);
            queueDiscoverSearch(discoverSearchFromMeta(kind, value));
            nav.onNavigate("discover");
          }}
        />
      ) : null}
    </div>
  );
}
