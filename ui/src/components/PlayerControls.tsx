import { Pause, Play, RotateCcw, RotateCw } from "lucide-react";
import { WaveformThrobber } from "@/components/WaveformThrobber";
import { SKIP_MS, type Player, type PlayerState } from "@/lib/player";
import { SPEEDS, formatClock, speedLabel } from "@/lib/playerTimeline";
import { cn } from "@/lib/utils";

/** Sleep timer choices, in minutes. */
const SLEEP_MINUTES = [5, 10, 15, 30, 45, 60];

const iconButton =
  "inline-flex items-center justify-center rounded-full text-ink transition-colors hover:bg-ink/10 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal disabled:opacity-40";

const selectClass =
  "rounded-md border border-ink/15 bg-card-strong px-2 py-1 text-xs font-semibold text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30";

/**
 * Back thirty seconds, play or pause, forward thirty seconds.
 *
 * @param props - The player, its state, and `large` for the now-playing view.
 */
export function TransportButtons({
  player,
  state,
  large = false,
}: {
  player: Player;
  state: PlayerState;
  large?: boolean;
}) {
  const playing = state.status === "playing";
  const seconds = SKIP_MS / 1000;
  const side = large ? "h-11 w-11" : "h-9 w-9";
  return (
    <div className={cn("flex items-center", large ? "gap-4" : "gap-1")}>
      <button
        type="button"
        className={cn(iconButton, side)}
        onClick={() => player.skip(-SKIP_MS)}
        aria-label={`Back ${seconds} seconds`}
        title={`Back ${seconds} seconds`}
      >
        <RotateCcw className={large ? "h-6 w-6" : "h-5 w-5"} />
      </button>
      <button
        type="button"
        className={cn(
          iconButton,
          "bg-ink text-paper hover:bg-ink-soft",
          large ? "h-16 w-16" : "h-10 w-10",
        )}
        onClick={() => player.toggle()}
        aria-label={playing ? "Pause" : "Play"}
        title={playing ? "Pause" : "Play"}
      >
        {state.status === "loading" ? (
          // As large as the play icon it stands in for, in a brick that
          // stands out on the ink button.
          <WaveformThrobber
            size="sm"
            className={cn("text-brick-on-ink", large ? "h-7 w-7" : "h-5 w-5")}
          />
        ) : playing ? (
          <Pause className={large ? "h-7 w-7" : "h-5 w-5"} />
        ) : (
          <Play className={cn(large ? "h-7 w-7" : "h-5 w-5", "translate-x-px")} />
        )}
      </button>
      <button
        type="button"
        className={cn(iconButton, side)}
        onClick={() => player.skip(SKIP_MS)}
        aria-label={`Forward ${seconds} seconds`}
        title={`Forward ${seconds} seconds`}
      >
        <RotateCw className={large ? "h-6 w-6" : "h-5 w-5"} />
      </button>
    </div>
  );
}

/**
 * The timeline slider. Disabled until the book's length is known.
 *
 * @param props - The player and its state.
 */
export function Scrubber({ player, state }: { player: Player; state: PlayerState }) {
  const duration = state.durationMs ?? 0;
  return (
    <input
      type="range"
      className="h-6 w-full cursor-pointer accent-teal disabled:cursor-default"
      min={0}
      max={Math.max(1, Math.round(duration / 1000))}
      step={1}
      value={Math.round(Math.min(state.positionMs, duration || state.positionMs) / 1000)}
      disabled={duration <= 0}
      onChange={(event) => player.seek(Number(event.target.value) * 1000)}
      aria-label="Position"
      aria-valuetext={`${formatClock(state.positionMs)} of ${formatClock(duration)}`}
    />
  );
}

/**
 * The speed menu.
 *
 * @param props - The player and its state.
 */
export function SpeedSelect({ player, state }: { player: Player; state: PlayerState }) {
  return (
    <select
      className={selectClass}
      value={state.speed}
      onChange={(event) => player.setSpeed(Number(event.target.value))}
      aria-label="Playback speed"
      title="Playback speed"
    >
      {SPEEDS.map((speed) => (
        <option key={speed} value={speed}>
          {speedLabel(speed)}
        </option>
      ))}
    </select>
  );
}

/**
 * The sleep timer menu. While a timer runs, its first entry shows the
 * minutes left.
 *
 * @param props - The player and its state.
 */
export function SleepSelect({ player, state }: { player: Player; state: PlayerState }) {
  const left = state.sleepAt === null ? null : player.sleepRemainingMs();
  return (
    <select
      className={selectClass}
      value={left === null ? "off" : "running"}
      onChange={(event) => {
        const value = event.target.value;
        if (value === "running") return;
        player.setSleep(value === "off" ? null : Number(value));
      }}
      aria-label="Sleep timer"
      title="Sleep timer"
    >
      {left !== null ? (
        <option value="running">Sleep in {Math.max(1, Math.ceil(left / 60_000))} min</option>
      ) : null}
      <option value="off">{left === null ? "Sleep timer" : "Turn off"}</option>
      {SLEEP_MINUTES.map((minutes) => (
        <option key={minutes} value={minutes}>
          {minutes} minutes
        </option>
      ))}
    </select>
  );
}
