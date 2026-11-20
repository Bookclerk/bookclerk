import { useCallback, useEffect, useState } from "react";
import { Play } from "lucide-react";
import { CoverThumb } from "@/components/CoverThumb";
import { fetchWork } from "@/lib/api";
import { useLiveRefresh } from "@/lib/live";
import { fetchPlaybackProgress } from "@/lib/playback";
import { formatRemaining } from "@/lib/playerTimeline";
import { percentDone, readShelf, type ShelfItem } from "@/lib/playTarget";
import { usePlayerState, webPlayer } from "@/lib/webPlayer";

/**
 * The Continue listening shelf: the books the listener is part way
 * through, most recent first. Renders nothing when there are none, or for
 * a caller with no listening state (the operator token).
 */
export function ContinueListening() {
  const [items, setItems] = useState<ShelfItem[]>([]);
  const { speed, target } = usePlayerState();
  const refresh = useCallback(async () => {
    try {
      setItems(await readShelf(fetchPlaybackProgress, fetchWork));
    } catch {
      // The shelf is a convenience: a failed read leaves what it showed.
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  useLiveRefresh(["playback"], refresh, { pollMs: 120_000 });

  if (items.length === 0) return null;
  return (
    <section aria-labelledby="continue-listening-heading" className="px-3 pb-2 pt-4 sm:px-4">
      <h2 id="continue-listening-heading" className="font-display text-base font-semibold text-ink">
        Continue listening
      </h2>
      <ul className="mt-2 flex gap-3 overflow-x-auto pb-2">
        {items.map(({ target: book, progress }) => {
          const percent = percentDone(progress);
          const left =
            progress.duration_ms !== null
              ? `${formatRemaining(progress.duration_ms - progress.position_ms, speed)} left`
              : "Started";
          return (
            <li key={book.editionId} className="w-36 shrink-0 sm:w-40">
              <button
                type="button"
                className="group block w-full text-left"
                onClick={() => void webPlayer().play(book)}
                aria-label={`Continue ${book.title}`}
              >
                <span className="relative block">
                  <CoverThumb
                    path={book.coverPath}
                    size={360}
                    className="h-36 w-36 rounded-md bg-fold shadow-sm sm:h-40 sm:w-40"
                  />
                  <span className="absolute inset-0 flex items-center justify-center rounded-md bg-scrim opacity-0 transition-opacity group-hover:opacity-100 group-focus-visible:opacity-100">
                    <Play className="h-9 w-9 text-white" />
                  </span>
                  <span
                    className="absolute inset-x-0 bottom-0 h-1 overflow-hidden rounded-b-md bg-ink/20"
                    role="progressbar"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={percent}
                    aria-label={`${percent}% listened`}
                  >
                    <span className="block h-full bg-teal" style={{ width: `${percent}%` }} />
                  </span>
                </span>
                <span className="mt-1.5 block truncate text-sm font-semibold text-ink">
                  {book.title}
                </span>
                <span className="block truncate text-xs text-ink/55">
                  {target?.editionId === book.editionId ? "Playing now" : left}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
