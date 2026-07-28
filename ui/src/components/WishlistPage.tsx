import { useEffect, useState } from "react";
import { LogOut, Trash2 } from "lucide-react";
import { AppNav, type AppNavProps } from "@/components/AppNav";
import { Button } from "@/components/ui/button";
import {
  fetchRequestQueue,
  fetchWishlist,
  removeWishlistItem,
  signOut,
  type AuthRole,
  type GlobalQueueEntry,
  type TitleRequest,
} from "@/lib/api";

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
  const [queue, setQueue] = useState<GlobalQueueEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function refresh() {
    setError(null);
    setBusy(true);
    try {
      const [w, q] = await Promise.all([fetchWishlist(), fetchRequestQueue()]);
      setMine(w);
      setQueue(q);
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

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  const myKeys = new Set(mine.map((r) => r.work_key).filter(Boolean));

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className="mx-auto flex max-w-6xl items-center justify-between gap-3">
          <div className="flex items-center gap-3 sm:gap-5">
            <img
              src="/bookclerk-logo.svg"
              alt="Bookclerk"
              className="h-8 w-auto sm:h-9"
            />
            <AppNav {...nav} />
          </div>
          <Button variant="ghost" onClick={() => void onSignOut()} aria-label="Sign out">
            <LogOut className="h-4 w-4" />
          </Button>
        </div>
      </header>

      <main className="mx-auto grid w-full max-w-6xl flex-1 gap-8 overflow-auto px-4 py-6 lg:grid-cols-[minmax(0,1fr)_280px]">
        <section className="min-w-0 space-y-4">
          <div className="space-y-1">
            <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">
              Your wishlist
            </h1>
            <p className="text-sm text-ink/60">
              Titles you want acquired. Un-wishlist to remove them from your list and the
              shared queue. Storefront is chosen when acquiring — not when wishlisting.
            </p>
          </div>

          {error ? (
            <p className="text-sm font-medium text-brick" role="alert">
              {error}
            </p>
          ) : null}

          {mine.length === 0 ? (
            <p className="text-sm text-ink/50">
              Nothing yet — wishlist from Discover cards or catalog search.
            </p>
          ) : (
            <ul className="divide-y divide-ink/10 border border-ink/10 bg-white/35">
              {mine.map((r) => (
                <li
                  key={r.uuid}
                  className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
                >
                  <div className="min-w-0">
                    <p className="font-medium text-ink">{r.title}</p>
                    <p className="text-xs text-ink/50">
                      {r.authors ?? "Unknown author"}
                      {r.asin ? ` · ${r.asin}` : ""}
                      {r.isbn ? ` · ${r.isbn}` : ""}
                    </p>
                  </div>
                  <Button
                    variant="ghost"
                    disabled={busy}
                    onClick={() => void onRemove(r.uuid)}
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
          {queue.length === 0 ? (
            <p className="text-sm text-ink/50">Queue is empty.</p>
          ) : (
            <ol className="space-y-3">
              {queue.map((entry, i) => (
                <li key={entry.work_key} className="text-sm">
                  <div className="flex items-baseline justify-between gap-2">
                    <span className="text-xs tabular-nums text-ink/40">{i + 1}</span>
                    <span className="min-w-0 flex-1 font-medium text-ink">{entry.title}</span>
                    <span className="shrink-0 tabular-nums text-xs font-semibold text-teal">
                      ×{entry.wish_count}
                    </span>
                  </div>
                  <p className="pl-5 text-xs text-ink/50">
                    {entry.authors ?? "Unknown author"}
                    {myKeys.has(entry.work_key) ? " · on your list" : ""}
                  </p>
                  {entry.reasons?.[0] ? (
                    <p className="pl-5 text-[11px] text-ink/40">{entry.reasons[0]}</p>
                  ) : null}
                </li>
              ))}
            </ol>
          )}
        </aside>
      </main>
    </div>
  );
}
