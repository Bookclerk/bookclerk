/**
 * The page's one {@link Player}, wired to the daemon's routes and the
 * browser, and its React binding.
 *
 * @module
 */
import { useSyncExternalStore } from "react";
import { beforeSignOut, coverSrc, fetchWork, type WorkDetail } from "@/lib/api";
import { bindMediaSession, type MediaDescription, type MediaSessionLike } from "@/lib/mediaSession";
import {
  addBookmark,
  closePlaybackSession,
  deleteBookmark,
  fetchPlaybackManifest,
  fetchPlaybackProgress,
  openPlaybackSession,
  putPlaybackProgress,
  syncPlaybackSession,
} from "@/lib/playback";
import { Player, type PlayerState } from "@/lib/player";
import { audioEditions, playableEdition, targetFor } from "@/lib/playTarget";

/** Where the chosen speed is kept between visits. */
const SPEED_KEY = "bookclerk.player.speed";

/**
 * A short name for this browser, shown in a listener's play history.
 *
 * @param userAgent - `navigator.userAgent`.
 * @returns Such as `Web (Firefox)`; `Web` when the browser is not recognized.
 */
export function playerName(userAgent: string): string {
  // Order matters: Edge and Opera also say Chrome, Chrome also says Safari.
  const known: [RegExp, string][] = [
    [/Edg\//, "Edge"],
    [/OPR\//, "Opera"],
    [/Firefox\//, "Firefox"],
    [/Chrome\//, "Chrome"],
    [/Safari\//, "Safari"],
  ];
  const match = known.find(([pattern]) => pattern.test(userAgent));
  return match ? `Web (${match[1]})` : "Web";
}

/** How long a track's metadata may take to arrive, in milliseconds. */
const PROBE_WAIT_MS = 15_000;

/**
 * How long Play waits for the listener's history to pick between several
 * editions of a work, in milliseconds.
 */
export const HISTORY_WAIT_MS = 2000;

/**
 * Reads a track's length from its stream's metadata.
 *
 * @param path - The track's stream path.
 * @returns Its length in milliseconds.
 */
function probe(path: string): Promise<number> {
  return new Promise((resolve, reject) => {
    const audio = new Audio();
    audio.preload = "metadata";
    const onMetadata = () => {
      const ms = audio.duration * 1000;
      done(() => (Number.isFinite(ms) ? resolve(ms) : reject(new Error("unknown length"))));
    };
    const onError = () => done(() => reject(new Error("unreadable track")));
    // A stream that stalls sends neither event.
    const timer = setTimeout(
      () => done(() => reject(new Error("the track did not answer"))),
      PROBE_WAIT_MS,
    );
    const done = (settle: () => void) => {
      clearTimeout(timer);
      audio.removeEventListener("loadedmetadata", onMetadata);
      audio.removeEventListener("error", onError);
      audio.removeAttribute("src");
      audio.load();
      settle();
    };
    audio.addEventListener("loadedmetadata", onMetadata);
    audio.addEventListener("error", onError);
    audio.src = path;
  });
}

/**
 * The cover images the OS media panel may show for a cover path.
 *
 * @param coverPath - A same-origin `cover_path`.
 * @returns Artwork entries; empty without a cover.
 */
export function coverArtwork(coverPath: string | null): MediaDescription["artwork"] {
  const small = coverSrc(coverPath, 128);
  const large = coverSrc(coverPath, 360);
  return [
    ...(small ? [{ src: small, sizes: "128x128" }] : []),
    ...(large ? [{ src: large, sizes: "360x360" }] : []),
  ];
}

/** The page's player, made on first use. */
let shared: Player | null = null;

/**
 * The page's player. The first call makes it and binds the browser's
 * Media Session and the page's departure to it.
 *
 * @returns The player.
 */
export function webPlayer(): Player {
  if (shared) return shared;
  const name = playerName(navigator.userAgent);
  const player = new Player({
    audio: () => {
      const audio = new Audio();
      audio.preload = "auto";
      return audio;
    },
    manifest: fetchPlaybackManifest,
    open: (editionId) => openPlaybackSession(editionId, name),
    sync: syncPlaybackSession,
    close: closePlaybackSession,
    addBookmark,
    deleteBookmark,
    setProgress: putPlaybackProgress,
    probe,
    now: () => performance.now(),
    wait: (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
    loadSpeed: () => {
      try {
        return window.localStorage.getItem(SPEED_KEY);
      } catch {
        return null;
      }
    },
    saveSpeed: (speed) => {
      try {
        window.localStorage.setItem(SPEED_KEY, String(speed));
      } catch {
        // Storage is off: the speed lasts for this visit.
      }
    },
  });
  const session = (navigator as { mediaSession?: MediaSessionLike }).mediaSession;
  bindMediaSession(player, session, (description) => new MediaMetadata(description), coverArtwork);
  // The last report must outlive the page, so it goes out as a keepalive
  // request when the page is hidden for good.
  window.addEventListener("pagehide", () => void player.stop(true));
  // Signing out revokes the session the close needs, so close first, and
  // start nothing until the identity has changed.
  beforeSignOut(
    () => player.retire(),
    () => player.readmit(),
  );
  shared = player;
  return player;
}

/**
 * The player's state, re-rendering the caller when it changes.
 *
 * @returns The current state.
 */
export function usePlayerState(): PlayerState {
  const player = webPlayer();
  return useSyncExternalStore(player.subscribe, () => player.state);
}

/**
 * Plays a work: the acquired audio edition the listener was last in, else
 * the first one.
 *
 * @param workId - The work.
 * @param known - The work's detail, when the caller already read it.
 * @returns `null` once playback was asked for; words for the listener
 *   when the work has no audio on this server.
 */
export async function playWork(workId: string, known?: WorkDetail | null): Promise<string | null> {
  const player = webPlayer();
  const latest = player.claim();
  // Only a preference between editions, read alongside the work: a read
  // that fails or is slow means the first edition.
  const history = fetchPlaybackProgress({ limit: 50, includeFinished: true })
    .then((page) => page.items)
    .catch(() => []);
  let work: WorkDetail;
  try {
    work = known ?? (await fetchWork(workId));
  } catch (err) {
    // A failure of a choice the listener has since replaced is not shown.
    if (!latest()) return null;
    throw err;
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  const recent =
    audioEditions(work).length > 1
      ? await Promise.race([
          history,
          new Promise<[]>((resolve) => {
            timer = setTimeout(() => resolve([]), HISTORY_WAIT_MS);
          }),
        ])
      : [];
  clearTimeout(timer);
  // The listener chose, started or stopped another book while this one
  // was being read: that later choice stands, and nothing is said of this.
  if (!latest()) return null;
  const edition = playableEdition(work, recent);
  if (!edition) return `No audio of “${work.title}” is on this server yet.`;
  void player.play(targetFor(work, edition));
  return null;
}
