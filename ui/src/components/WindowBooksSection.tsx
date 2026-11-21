import { useCallback, useEffect, useRef, useState } from "react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { CoverThumb } from "@/components/CoverThumb";
import { StoreLogo } from "@/components/StoreLogo";
import { storeLabel } from "@/lib/catalogTitle";
import { ApiError } from "@/lib/api";
import { clerkRequestFailure, requestFromLibrary } from "@/lib/clerk";
import {
  fetchWindowBooks,
  shownWindowBooks,
  windowEntryFormats,
  windowEntryIsYours,
  windowEntryLibraries,
  windowEntryRequest,
  windowLibraries,
  type WindowBooks,
  type WindowEntryView,
  type WindowRequest,
} from "@/lib/join";
import { useLiveRefresh } from "@/lib/live";

/** Where a request to a library stands. */
type RequestState =
  { kind: "idle" } | { kind: "sending" } | { kind: "sent" } | { kind: "failed"; message: string };

/**
 * Asks the library for a book only its branches hold: the library fetches
 * it from a branch, as for a request from the person's browser there.
 */
function RequestButton({ ask }: { ask: WindowRequest }) {
  const [state, setState] = useState<RequestState>({ kind: "idle" });
  const send = async () => {
    setState({ kind: "sending" });
    try {
      await requestFromLibrary(ask.libraryId, ask.workId);
      setState({ kind: "sent" });
    } catch (err) {
      setState({ kind: "failed", message: clerkRequestFailure(err) });
    }
  };
  if (state.kind === "sent") {
    return (
      <p className="mt-1 text-xs text-ink/70" role="status">
        Requested. It shows with your requests on {ask.host}.
      </p>
    );
  }
  return (
    <div className="mt-1 flex flex-wrap items-center gap-2">
      <Button
        variant="secondary"
        className="h-7 px-2 text-xs"
        disabled={state.kind === "sending"}
        onClick={() => void send()}
      >
        {state.kind === "sending" ? "Requesting…" : "Request"}
      </Button>
      {state.kind === "failed" ? (
        <span className="text-xs text-ink/70" role="alert">
          {state.message}
        </span>
      ) : null}
    </div>
  );
}

/**
 * One work another library holds, marked Yours when it is the person's,
 * or one only that library's branches hold, with a Request button.
 */
function WindowRow({ entry }: { entry: WindowEntryView }) {
  const first = entry.editions[0];
  if (!first) return null;
  const ask = windowEntryRequest(entry);
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
          {windowEntryIsYours(entry) ? <Badge className="bg-ink text-paper">Yours</Badge> : null}
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
          <span>{ask ? `· On a branch of ${ask.host}` : `· In ${libraries.join(", ")}`}</span>
          {series ? <span>· {series}</span> : null}
        </p>
        {ask ? <RequestButton key={`${ask.libraryId}/${ask.workId}`} ask={ask} /> : null}
      </div>
    </div>
  );
}

/**
 * Books in other libraries: every book of each library this server is a
 * branch of or holds a library card for,
 * merged by work, the person's own marked Yours, narrowed by the Library
 * search and, when asked, to the person's own. Renders
 * nothing when this server reads no other library, or the caller may not
 * read the view (only the
 * operator or an Owner may). A refresh that fails for another reason keeps
 * the list it has and offers to try again.
 */
export function WindowBooksSection({ query = "" }: { query?: string }) {
  const [view, setView] = useState<WindowBooks | null>(null);
  const [stale, setStale] = useState(false);
  // "Only yours": a per-visit choice, off so every book shows by default.
  const [onlyYours, setOnlyYours] = useState(false);
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
  // Poll every minute while the live socket is down (a proxy without
  // WebSocket forwarding), so books the window reader stores still show.
  useLiveRefresh(["libraries"], load, { pollMs: 60_000, openPollMs: 300_000 });

  if (!view) return null;
  const libraries = windowLibraries(view);
  if (libraries.length === 0) return null;
  const shown = shownWindowBooks(view.books, query, onlyYours);
  const anyYours = view.books.some(windowEntryIsYours);
  return (
    <section aria-labelledby="window-books-heading" className="bg-card">
      <div className="border-b border-ink/10 px-3 py-3 sm:px-4">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <h2 id="window-books-heading" className="font-display text-lg font-semibold text-ink">
            Books in other libraries
          </h2>
          {anyYours ? (
            <label className="flex items-center gap-2 text-xs text-ink/70">
              <input
                type="checkbox"
                checked={onlyYours}
                onChange={(e) => setOnlyYours(e.target.checked)}
              />
              Only yours
            </label>
          ) : null}
        </div>
        <p className="text-xs text-ink/55">
          Every book in {libraries.join(", ")}; the ones on your own store accounts there are marked
          Yours. Books only a library&apos;s branches hold can be requested from it.
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
          {onlyYours
            ? "None of your books in other libraries match this search."
            : "No books in other libraries match this search."}
        </p>
      ) : (
        shown.map((entry) => <WindowRow key={entry.key} entry={entry} />)
      )}
    </section>
  );
}
