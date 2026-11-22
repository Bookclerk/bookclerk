import { useEffect, useState } from "react";
import { ChevronUp, X } from "lucide-react";
import { CoverThumb } from "@/components/CoverThumb";
import { NowPlaying } from "@/components/NowPlaying";
import { Scrubber, SleepSelect, SpeedSelect, TransportButtons } from "@/components/PlayerControls";
import { formatClock } from "@/lib/playerTimeline";
import type { Player, PlayerState } from "@/lib/player";
import { cn, pageWidthClass } from "@/lib/utils";
import { usePlayerState, webPlayer } from "@/lib/webPlayer";

/**
 * The player bar along the bottom of every screen while a book is loaded,
 * and the now-playing view it opens. Renders nothing with no book loaded.
 *
 * Leaving the signed-in app (signing out) stops the book.
 */
export function PlayerBar() {
  const player = webPlayer();
  const state = usePlayerState();
  const [expanded, setExpanded] = useState(false);
  useEffect(() => () => void player.stop(), [player]);

  return (
    <>
      <PlayerBarView player={player} state={state} onOpen={() => setExpanded(true)} />
      {expanded ? <NowPlaying onClose={() => setExpanded(false)} /> : null}
    </>
  );
}

/**
 * The player bar itself, drawn from `state`: the book, the transport
 * buttons, the clock and the speed and sleep menus on one row over the
 * timeline. {@link PlayerBar} gives it the page's player; a story gives it
 * a fixed one.
 *
 * @param props - `player` takes the controls' commands, `state` is what
 *   they show, and `onOpen` opens the now-playing view.
 * @returns The bar, or nothing with no book loaded.
 */
export function PlayerBarView({
  player,
  state,
  onOpen,
}: {
  player: Player;
  state: PlayerState;
  onOpen: () => void;
}) {
  const target = state.target;
  if (!target) return null;
  return (
    <section
      aria-label="Player"
      className="shrink-0 border-t border-ink/15 bg-paper/95 px-3 py-2 shadow-[0_-4px_16px_rgba(11,53,83,0.08)] backdrop-blur-md sm:px-5"
    >
      <div className={cn("flex flex-col", pageWidthClass)}>
        <div className="flex items-center gap-3">
          <button
            type="button"
            className="flex min-w-0 flex-1 items-center gap-3 text-left sm:flex-none sm:basis-1/3"
            onClick={onOpen}
          >
            <span className="sr-only">Open the player:</span>
            <CoverThumb
              path={target.coverPath}
              className="h-11 w-11 shrink-0 rounded-md bg-fold shadow-sm"
            />
            <span className="min-w-0">
              <span className="block truncate font-display text-sm font-semibold text-ink">
                {target.title}
              </span>
              <span className="block truncate text-xs text-ink/70">
                {state.error ?? target.authors ?? ""}
              </span>
            </span>
          </button>
          <div className="flex shrink-0 items-center sm:flex-1 sm:justify-center">
            <TransportButtons player={player} state={state} />
          </div>
          <div className="flex shrink-0 items-center justify-end gap-2 sm:basis-1/3">
            <span className="hidden text-xs tabular-nums text-ink/70 md:inline">
              {formatClock(state.positionMs)}
              {state.durationMs ? ` / ${formatClock(state.durationMs)}` : ""}
            </span>
            <span className="hidden items-center gap-2 lg:flex">
              <SpeedSelect player={player} state={state} />
              <SleepSelect player={player} state={state} />
            </span>
            <button
              type="button"
              className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
              onClick={onOpen}
              aria-label="Open the player"
              title="Open the player"
            >
              <ChevronUp className="h-5 w-5" />
            </button>
            <button
              type="button"
              className="rounded-md p-1.5 text-ink/70 transition-colors hover:bg-ink/5 hover:text-ink"
              onClick={() => void player.stop()}
              aria-label="Stop and close the player"
              title="Stop and close the player"
            >
              <X className="h-5 w-5" />
            </button>
          </div>
        </div>
        <Scrubber player={player} state={state} />
      </div>
    </section>
  );
}
