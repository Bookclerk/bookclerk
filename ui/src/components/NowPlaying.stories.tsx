/**
 * Stories for {@link NowPlayingView}, the now-playing view the player bar
 * opens over the app, in each state that looks different.
 */
import type { Meta, StoryObj } from "@storybook/react-vite";
import type { Bookmark } from "@/lib/playback";
import type { PlayerState } from "@/lib/player";
import { NowPlayingView } from "./NowPlaying";
import { book, paused, playerIn } from "./playerStoryState";

/**
 * The view over `state`, as the player bar opens it.
 *
 * @param props - The state to show.
 */
function View({ state }: { state: PlayerState }) {
  return <NowPlayingView player={playerIn(state)} state={state} onClose={() => {}} />;
}

/**
 * A bookmark in the story's edition.
 *
 * @param id - Its id.
 * @param minutes - Its place, in minutes from the start.
 * @param title - Its note.
 * @returns The bookmark.
 */
function bookmark(id: string, minutes: number, title: string): Bookmark {
  return {
    id,
    edition_id: book.editionId,
    position_ms: minutes * 60_000,
    title,
    created_at: "2026-11-01T12:00:00Z",
  };
}

const meta = {
  title: "Player/NowPlaying",
  component: View,
  args: { state: paused },
  // The view covers the whole screen, centred on wide ones and a sheet from
  // the bottom on narrow ones.
  parameters: { layout: "fullscreen" },
} satisfies Meta<typeof View>;

export default meta;

/** A story of the now-playing view. */
type Story = StoryObj<typeof meta>;

/** Paused part way through, with no bookmarks yet. */
export const Paused: Story = {};

/** Playing faster than normal, with a sleep timer and two bookmarks. */
export const PlayingWithBookmarks: Story = {
  args: {
    state: {
      ...paused,
      status: "playing",
      speed: 1.5,
      sleepAt: 12 * 60_000,
      bookmarks: [bookmark("b1", 12, "Where the map turns up"), bookmark("b2", 38, "")],
    },
  },
};

/** A book with parts and chapters, part way through the second chapter. */
export const WithChapters: Story = {
  args: {
    state: {
      ...paused,
      chapters: [
        { title: "Part One", start_ms: 0, end_ms: 0, depth: 0 },
        { title: "The Harbour", start_ms: 0, end_ms: 25 * 60_000, depth: 1 },
        { title: "The Map", start_ms: 25 * 60_000, end_ms: 70 * 60_000, depth: 1 },
        { title: "Part Two", start_ms: 70 * 60_000, end_ms: 10 * 3_600_000, depth: 0 },
      ],
    },
  },
};

/** Waiting for the stream: the middle button shows the throbber. */
export const Loading: Story = { args: { state: { ...paused, status: "loading" } } };

/** Played to the end. */
export const Finished: Story = {
  args: { state: { ...paused, positionMs: paused.durationMs ?? 0, finished: true } },
};
