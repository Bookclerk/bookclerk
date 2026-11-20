/**
 * Binds the web player to the browser's Media Session, which is what puts
 * the book on the lock screen, in the OS media panel and under the
 * keyboard's media keys.
 *
 * @module
 */
import { SKIP_MS, type Player, type PlayerState } from "@/lib/player";

/** The actions the binding answers. */
export const MEDIA_ACTIONS = [
  "play",
  "pause",
  "stop",
  "seekbackward",
  "seekforward",
  "seekto",
  "previoustrack",
  "nexttrack",
] as const;

/** One of {@link MEDIA_ACTIONS}. */
export type MediaAction = (typeof MEDIA_ACTIONS)[number];

/** What an action handler is told (a subset of `MediaSessionActionDetails`). */
export interface MediaActionDetails {
  /** Seconds to move, for a seek back or forward. */
  seekOffset?: number | null;
  /** Seconds from the start, for a seek to a place. */
  seekTime?: number | null;
}

/** What the binding shows the OS about the loaded book. */
export interface MediaDescription {
  /** Book title. */
  title: string;
  /** Authors; empty when unknown. */
  artist: string;
  /** Cover images. */
  artwork: { src: string; sizes: string; type?: string }[];
}

/** The part of `navigator.mediaSession` the binding uses. */
export interface MediaSessionLike {
  /** What is playing; `null` for nothing. */
  metadata: unknown;
  /** Whether it is playing. */
  playbackState: "none" | "paused" | "playing";
  /** Registers or clears an action's handler. */
  setActionHandler(
    action: MediaAction,
    handler: ((details: MediaActionDetails) => void) | null,
  ): void;
  /** Tells the OS the position, for its own scrubber. */
  setPositionState?(state?: { duration: number; playbackRate: number; position: number }): void;
}

/**
 * Keeps a Media Session in step with a player, and routes the session's
 * actions to it.
 *
 * Previous and next track skip thirty seconds, since headsets and car
 * buttons often send only those.
 *
 * @param player - The player.
 * @param session - `navigator.mediaSession`; `undefined` where the browser has none.
 * @param describe - Turns a description into the session's metadata (`new MediaMetadata(...)`).
 * @param covers - The cover images for a same-origin cover path.
 * @returns Unbinds: clears the handlers and the metadata.
 */
export function bindMediaSession(
  player: Player,
  session: MediaSessionLike | undefined,
  describe: (description: MediaDescription) => unknown,
  covers: (coverPath: string | null) => MediaDescription["artwork"],
): () => void {
  if (!session) return () => undefined;
  const handlers: Record<MediaAction, (details: MediaActionDetails) => void> = {
    play: () => player.resume(),
    pause: () => player.pause(),
    stop: () => void player.stop(),
    seekbackward: (details) => player.skip(-(details.seekOffset ?? SKIP_MS / 1000) * 1000),
    seekforward: (details) => player.skip((details.seekOffset ?? SKIP_MS / 1000) * 1000),
    seekto: (details) => {
      if (typeof details.seekTime === "number") player.seek(details.seekTime * 1000);
    },
    previoustrack: () => player.skip(-SKIP_MS),
    nexttrack: () => player.skip(SKIP_MS),
  };
  for (const action of MEDIA_ACTIONS) {
    try {
      session.setActionHandler(action, handlers[action]);
    } catch {
      // A browser that does not know an action refuses it; the rest stand.
    }
  }

  let shown: PlayerState["target"] = null;
  let sent = { position: -1, duration: -1, rate: -1 };
  const update = () => {
    const state = player.state;
    if (state.target !== shown) {
      shown = state.target;
      session.metadata = shown
        ? describe({
            title: shown.title,
            artist: shown.authors ?? "",
            artwork: covers(shown.coverPath),
          })
        : null;
    }
    session.playbackState =
      state.status === "playing" ? "playing" : state.target ? "paused" : "none";
    if (!session.setPositionState) return;
    const duration = (state.durationMs ?? 0) / 1000;
    const position = Math.min(state.positionMs / 1000, duration);
    if (!state.target || duration <= 0) {
      try {
        if (sent.duration !== 0) session.setPositionState();
      } catch {
        // Some browsers refuse to clear the position; it goes with the metadata.
      }
      sent = { position: 0, duration: 0, rate: 0 };
      return;
    }
    // The OS runs its own clock from the last state, so tell it only
    // about changes it could not predict.
    const expected = state.status === "playing" ? 2 : 0.5;
    if (
      sent.duration === duration &&
      sent.rate === state.speed &&
      Math.abs(position - sent.position) < expected
    ) {
      return;
    }
    sent = { position, duration, rate: state.speed };
    try {
      session.setPositionState({ duration, playbackRate: state.speed, position });
    } catch {
      // A position past the duration is refused; the next update corrects it.
    }
  };
  const unsubscribe = player.subscribe(update);
  update();
  return () => {
    unsubscribe();
    for (const action of MEDIA_ACTIONS) {
      try {
        session.setActionHandler(action, null);
      } catch {
        // See above.
      }
    }
    session.metadata = null;
    session.playbackState = "none";
  };
}
