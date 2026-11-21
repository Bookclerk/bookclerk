/**
 * Fixed player states for the player's stories: an engine that calls no
 * route and plays nothing, and a book paused part way through.
 *
 * Only stories import this module, so it never reaches the app's bundle.
 *
 * @module
 */
import { Player, type PlayTarget, type PlayerDeps, type PlayerState } from "@/lib/player";

/** A route no story calls. */
const unused = () => Promise.reject(new Error("a story calls no route"));

/** Routes that answer nothing and a clock that stands still. */
const deps: PlayerDeps = {
  audio: () => {
    throw new Error("a story plays nothing");
  },
  manifest: unused,
  open: unused,
  sync: unused,
  close: unused,
  addBookmark: unused,
  deleteBookmark: unused,
  setProgress: unused,
  probe: unused,
  now: () => 0,
  wait: () => Promise.resolve(),
  loadSpeed: () => null,
  saveSpeed: () => {},
};

/** A book with no cover, so a story loads no image. */
export const book: PlayTarget = {
  editionId: "edition-1",
  workId: "work-1",
  title: "The Long Way Round",
  authors: "Avery Lindqvist",
  coverPath: null,
};

/** {@link book} paused forty minutes into ten hours. */
export const paused: PlayerState = {
  status: "paused",
  target: book,
  positionMs: 40 * 60_000,
  durationMs: 10 * 3_600_000,
  speed: 1,
  sleepAt: null,
  bookmarks: [],
  finished: false,
  error: null,
};

/**
 * An engine holding `state`, which the player's views draw.
 *
 * @param state - The state to show.
 * @returns The engine.
 */
export function playerIn(state: PlayerState): Player {
  const player = new Player(deps);
  player.state = state;
  return player;
}
