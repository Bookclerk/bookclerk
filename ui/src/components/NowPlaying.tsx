import { useEffect, useRef, useState } from "react";
import { BookmarkPlus, Check, ChevronDown, SkipBack, SkipForward, Trash2 } from "lucide-react";
import { CoverThumb } from "@/components/CoverThumb";
import { Scrubber, SleepSelect, SpeedSelect, TransportButtons } from "@/components/PlayerControls";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { userFacingApiError } from "@/lib/api";
import type { Player, PlayerState } from "@/lib/player";
import {
  chapterAt,
  formatClock,
  formatRemaining,
  nextChapterStart,
  previousChapterStart,
} from "@/lib/playerTimeline";
import { usePlayerState, webPlayer } from "@/lib/webPlayer";

/** What Tab stops at inside the now-playing view. */
const TAB_STOPS =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])';

/**
 * The now-playing view: the loaded book's cover, timeline, controls and
 * the edition's chapters and the listener's bookmarks. Closes itself when the book is unloaded.
 * While it is open the keyboard stays inside it; closing it returns to
 * whatever opened it.
 *
 * @param props - `onClose` returns to the player bar.
 */
export function NowPlaying({ onClose }: { onClose: () => void }) {
  return <NowPlayingView player={webPlayer()} state={usePlayerState()} onClose={onClose} />;
}

/**
 * The now-playing view drawn from `state`. {@link NowPlaying} gives it the
 * page's player; a story gives it a fixed one.
 *
 * @param props - `player` takes the view's commands, `state` is what it
 *   shows, and `onClose` returns to the player bar.
 * @returns The view, or nothing with no book loaded.
 */
export function NowPlayingView({
  player,
  state,
  onClose,
}: {
  player: Player;
  state: PlayerState;
  onClose: () => void;
}) {
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<string | null>(null);
  const target = state.target;
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!target) onClose();
  }, [target, onClose]);
  useEffect(() => {
    const opener = document.activeElement;
    panelRef.current?.focus();
    return () => {
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
    };
  }, []);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
      if (event.key !== "Tab") return;
      // Tab goes round the view and never to the page under it.
      const panel = panelRef.current;
      if (!panel) return;
      const stops = [...panel.querySelectorAll<HTMLElement>(TAB_STOPS)];
      const first = stops[0];
      const last = stops[stops.length - 1];
      const active = document.activeElement;
      let next: HTMLElement | null = null;
      if (!first || !last) next = panel;
      else if (!panel.contains(active)) next = first;
      else if (event.shiftKey && (active === first || active === panel)) next = last;
      else if (!event.shiftKey && active === last) next = first;
      if (next) {
        event.preventDefault();
        next.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  if (!target) return null;

  /** Runs a player request, showing its failure. */
  async function run(action: () => Promise<void>, fallback: string) {
    setBusy(true);
    setFailure(null);
    try {
      await action();
    } catch (err) {
      setFailure(userFacingApiError(err, fallback));
    } finally {
      setBusy(false);
    }
  }

  const duration = state.durationMs;
  const left = duration ? Math.max(0, duration - state.positionMs) : null;
  const current = chapterAt(state.chapters, state.positionMs);
  const nextStart = nextChapterStart(state.chapters, state.positionMs);

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-scrim p-0 backdrop-blur-[2px] sm:items-center sm:p-6"
      role="presentation"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="now-playing-heading"
        ref={panelRef}
        tabIndex={-1}
        className="outline-none flex max-h-[94vh] w-full max-w-xl flex-col overflow-hidden rounded-t-xl bg-paper shadow-xl sm:rounded-xl"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-ink/10 px-4 py-2.5">
          <p className="text-xs font-semibold uppercase tracking-wide text-ink/70">Now playing</p>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
            aria-label="Close"
          >
            <ChevronDown className="h-5 w-5" />
          </button>
        </div>
        <div className="flex flex-col gap-4 overflow-y-auto px-5 py-5 sm:px-8">
          <CoverThumb
            path={target.coverPath}
            size={360}
            className="mx-auto h-48 w-48 rounded-lg bg-fold shadow-md sm:h-56 sm:w-56"
          />
          <div className="text-center">
            <h2
              id="now-playing-heading"
              className="font-display text-xl font-bold leading-tight text-ink"
            >
              {target.title}
            </h2>
            {target.authors ? <p className="mt-1 text-sm text-ink/70">{target.authors}</p> : null}
            {current >= 0 ? (
              <p className="mt-1 truncate text-sm font-medium text-ink-soft">
                {state.chapters[current].title}
              </p>
            ) : null}
          </div>

          <div>
            <Scrubber player={player} state={state} />
            <div className="mt-1 flex justify-between text-xs tabular-nums text-ink/70">
              <span>{formatClock(state.positionMs)}</span>
              <span>{left !== null ? `${formatRemaining(left, state.speed)} left` : ""}</span>
              <span>{duration ? formatClock(duration) : "--:--"}</span>
            </div>
          </div>

          <div className="flex justify-center">
            <TransportButtons player={player} state={state} large />
          </div>
          <div className="flex flex-wrap items-center justify-center gap-2">
            <SpeedSelect player={player} state={state} />
            <SleepSelect player={player} state={state} />
            <Button
              variant="secondary"
              className="px-2.5 py-1 text-xs"
              disabled={busy}
              onClick={() => void run(() => player.markFinished(), "Could not mark it finished.")}
            >
              <Check className="h-4 w-4" />
              Mark finished
            </Button>
          </div>

          {state.finished ? (
            <p className="text-center text-sm font-medium text-ink" role="status">
              You finished this book.
            </p>
          ) : null}
          {(state.error ?? failure) ? (
            <p className="text-center text-sm font-medium text-brick-text" role="alert">
              {state.error ?? failure}
            </p>
          ) : null}

          {state.chapters.length > 0 ? (
            <section aria-labelledby="chapters-heading" className="border-t border-ink/10 pt-4">
              <div className="flex items-center justify-between gap-2">
                <h3 id="chapters-heading" className="text-sm font-semibold text-ink">
                  Chapters
                </h3>
                <div className="flex gap-1">
                  <button
                    type="button"
                    className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
                    onClick={() =>
                      player.seek(previousChapterStart(state.chapters, state.positionMs))
                    }
                    aria-label="Previous chapter"
                  >
                    <SkipBack className="h-4 w-4" />
                  </button>
                  <button
                    type="button"
                    className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink disabled:opacity-40"
                    disabled={nextStart === null}
                    onClick={() => {
                      if (nextStart !== null) player.seek(nextStart);
                    }}
                    aria-label="Next chapter"
                  >
                    <SkipForward className="h-4 w-4" />
                  </button>
                </div>
              </div>
              <ol className="mt-2 max-h-64 overflow-y-auto">
                {state.chapters.map((chapter, index) => (
                  <li key={`${index}-${chapter.start_ms}`}>
                    <button
                      type="button"
                      aria-current={index === current ? "true" : undefined}
                      className={`flex w-full items-baseline gap-3 rounded-md px-1 py-1.5 text-left hover:bg-ink/5 ${
                        index === current ? "bg-ink/5 font-semibold" : ""
                      }`}
                      style={{ paddingLeft: `${0.25 + Math.min(chapter.depth, 7) * 1}rem` }}
                      onClick={() => player.seek(chapter.start_ms)}
                    >
                      <span className="sr-only">Go to</span>
                      <span className="min-w-0 flex-1 truncate text-sm text-ink">
                        {chapter.title}
                      </span>
                      <span className="shrink-0 text-xs tabular-nums text-ink/70">
                        {formatClock(chapter.start_ms)}
                      </span>
                    </button>
                  </li>
                ))}
              </ol>
            </section>
          ) : null}

          <section aria-labelledby="bookmarks-heading" className="border-t border-ink/10 pt-4">
            <h3 id="bookmarks-heading" className="text-sm font-semibold text-ink">
              Bookmarks
            </h3>
            <form
              className="mt-2 flex gap-2"
              onSubmit={(event) => {
                event.preventDefault();
                // The note cannot be edited while it is sent, so clearing
                // it afterwards clears only what was saved.
                void run(async () => {
                  await player.addBookmark(note.trim());
                  setNote("");
                }, "Could not add the bookmark.");
              }}
            >
              <Input
                value={note}
                onChange={(event) => setNote(event.target.value)}
                placeholder={`Note for ${formatClock(state.positionMs)} (optional)`}
                maxLength={200}
                disabled={busy}
                aria-label="Bookmark note"
              />
              <Button type="submit" variant="secondary" className="shrink-0" disabled={busy}>
                <BookmarkPlus className="h-4 w-4" />
                Add
              </Button>
            </form>
            {state.bookmarks.length === 0 ? (
              <p className="mt-3 text-sm text-ink/70">
                No bookmarks yet. Add one to come back to this place.
              </p>
            ) : (
              <ul className="mt-2 divide-y divide-ink/10">
                {state.bookmarks.map((bookmark) => (
                  <li key={bookmark.id} className="flex items-center gap-2 py-1.5">
                    <button
                      type="button"
                      className="flex min-w-0 flex-1 items-baseline gap-3 rounded-md px-1 py-1 text-left hover:bg-ink/5"
                      onClick={() => player.seek(bookmark.position_ms)}
                    >
                      <span className="sr-only">Go to</span>
                      <span className="shrink-0 text-sm font-semibold tabular-nums text-ink-soft">
                        {formatClock(bookmark.position_ms)}
                      </span>
                      {bookmark.title ? <span className="sr-only">,</span> : null}
                      <span className="truncate text-sm text-ink/75">{bookmark.title}</span>
                    </button>
                    <button
                      type="button"
                      className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-brick/10 hover:text-brick-text"
                      disabled={busy}
                      onClick={() =>
                        void run(
                          () => player.removeBookmark(bookmark.id),
                          "Could not remove the bookmark.",
                        )
                      }
                      aria-label={`Remove the bookmark at ${formatClock(bookmark.position_ms)}`}
                    >
                      <Trash2 className="h-4 w-4" />
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}
