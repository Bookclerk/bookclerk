import { useCallback, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CoverThumb } from "@/components/CoverThumb";
import { StoreLogo } from "@/components/StoreLogo";
import { storeLabel } from "@/lib/catalogTitle";
import { ApiError } from "@/lib/api";
import {
  fetchWindowBooks,
  filterWindowBooks,
  windowEntryFormats,
  windowEntryLibraries,
  type WindowBooks,
  type WindowEntryView,
} from "@/lib/join";
import { useLiveRefresh } from "@/lib/live";

/** One work a library holds for this server's person. */
function WindowRow({ entry }: { entry: WindowEntryView }) {
  const first = entry.editions[0];
  if (!first) return null;
  const meta = [first.authors, first.narrators ? `narr. ${first.narrators}` : null]
    .filter(Boolean)
    .join(" · ");
  const series = first.series
    ? `${first.series}${first.series_index ? ` #${first.series_index}` : ""}`
    : null;
  const stores = [...new Set(entry.editions.map((e) => e.store))];
  const libraries = windowEntryLibraries(entry);
  return (
    <div className="grid grid-cols-[56px_1fr] items-center gap-3 border-b border-ink/10 px-3 py-2.5 sm:grid-cols-[64px_1fr] sm:gap-4 sm:px-4">
      <div className="relative h-14 w-14 overflow-hidden rounded-md bg-fold shadow-sm sm:h-16 sm:w-16">
        <CoverThumb className="h-full w-full rounded-none bg-transparent shadow-none" />
      </div>
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h3 className="truncate font-display text-base font-semibold leading-tight text-ink sm:text-lg">
            {first.title}
          </h3>
          {windowEntryFormats(entry).map((format) => (
            <Badge key={format} className="bg-teal/10 text-ink/70">
              {format === "pdf" ? "PDF" : "Audio"}
            </Badge>
          ))}
        </div>
        {meta ? <p className="mt-0.5 truncate text-sm text-ink/70">{meta}</p> : null}
        <p className="mt-0.5 flex flex-wrap items-center gap-1.5 text-xs text-ink/50">
          {stores.map((store) => (
            <span key={store} className="inline-flex items-center gap-1">
              <StoreLogo source={store} className="h-4 w-4" />
              {storeLabel(store)}
            </span>
          ))}
          <span>· In {libraries.join(", ")}</span>
          {series ? <span>· {series}</span> : null}
        </p>
      </div>
    </div>
  );
}

/**
 * Library on a window: the books each library this server is a window of
 * ("Use this computer to download" off) holds for its person, merged by
 * work, narrowed by the Library search. Renders nothing when this server
 * is a window of no library, or the caller may not read the view (only the
 * operator or an Owner may). A refresh that fails for another reason keeps
 * the list it has and offers to try again.
 */
export function WindowBooksSection({ query = "" }: { query?: string }) {
  const [view, setView] = useState<WindowBooks | null>(null);
  const [stale, setStale] = useState(false);
  // Only the latest refresh may change the view, so an older answer that
  // arrives late never replaces a newer one.
  const latest = useRef(0);

  const load = useCallback(async () => {
    const request = ++latest.current;
    try {
      const next = await fetchWindowBooks();
      if (request !== latest.current) return;
      setView(next);
      setStale(false);
    } catch (err) {
      if (request !== latest.current) return;
      if (err instanceof ApiError && (err.status === 401 || err.status === 403)) {
        // Not allowed: show nothing over the server's own library.
        setView(null);
        setStale(false);
      } else {
        // Not reachable now: keep what was read and offer a retry.
        setStale(true);
      }
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);
  useLiveRefresh(["clusters"], load, { pollMs: 0, openPollMs: 300_000 });

  if (!view || view.clusters.length === 0) return null;
  const shown = filterWindowBooks(view.books, query);
  const libraries = view.clusters.map((link) => {
    try {
      return new URL(link.owner_origin).host || link.owner_origin;
    } catch {
      return link.owner_origin;
    }
  });
  return (
    <section aria-labelledby="window-books-heading" className="bg-card">
      <div className="border-b border-ink/10 px-3 py-3 sm:px-4">
        <h2 id="window-books-heading" className="font-display text-lg font-semibold text-ink">
          Your books in other libraries
        </h2>
        <p className="text-xs text-ink/55">
          Held for you in {libraries.join(", ")}. This server doesn&apos;t download them; turn on
          &ldquo;Use this computer to download&rdquo; in Settings to keep copies here.
        </p>
      </div>
      {stale ? (
        <div
          className="flex flex-wrap items-center gap-2 border-b border-ink/10 px-3 py-2 text-xs text-ink/60 sm:px-4"
          role="status"
        >
          <span>Couldn&apos;t refresh this list; showing the last one read.</span>
          <Button variant="secondary" className="h-7 px-2 text-xs" onClick={() => void load()}>
            Try again
          </Button>
        </div>
      ) : null}
      {view.books.length === 0 ? (
        <p className="px-4 py-6 text-center text-sm text-ink/60">
          No books there yet, or the list hasn&apos;t been read yet.
        </p>
      ) : shown.length === 0 ? (
        <p className="px-4 py-6 text-center text-sm text-ink/60">
          None of your books in other libraries match this search.
        </p>
      ) : (
        shown.map((entry) => <WindowRow key={entry.key} entry={entry} />)
      )}
    </section>
  );
}
