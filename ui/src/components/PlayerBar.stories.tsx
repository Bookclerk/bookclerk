/**
 * Stories for {@link PlayerBarView}, the player bar along the bottom of the
 * app (and of the desktop window), in each state that looks different.
 *
 * The bar fills the story's width (inside the frame's padding) as it fills
 * the app's, so the `desktop` screenshot shows the book, transport
 * buttons, clock and menus on one row over the timeline, and the `phone`
 * one the narrow bar without the menus.
 */
import type { Meta, StoryObj } from "@storybook/react-vite";
import type { PlayerState } from "@/lib/player";
import { PlayerBarView } from "./PlayerBar";
import { book, paused, playerIn } from "./playerStoryState";

/**
 * The bar over `state`, as the app draws it.
 *
 * @param props - The state to show.
 */
function Bar({ state }: { state: PlayerState }) {
  return <PlayerBarView player={playerIn(state)} state={state} onOpen={() => {}} />;
}

const meta = {
  title: "Player/PlayerBar",
  component: Bar,
  args: { state: paused },
} satisfies Meta<typeof Bar>;

export default meta;

/** A story of the player bar. */
type Story = StoryObj<typeof meta>;

/** Paused part way through: the play button shows. */
export const Paused: Story = {};

/** Playing: the pause button shows. */
export const Playing: Story = { args: { state: { ...paused, status: "playing" } } };

/** Waiting for the stream: the middle button shows the throbber. */
export const Loading: Story = { args: { state: { ...paused, status: "loading" } } };

/** Length not yet known: the timeline is disabled and the clock shows no length. */
export const UnknownLength: Story = {
  args: { state: { ...paused, positionMs: 0, durationMs: null } },
};

/** Faster than normal, with a sleep timer twelve minutes from pausing. */
export const FastWithSleepTimer: Story = {
  args: { state: { ...paused, status: "playing", speed: 1.5, sleepAt: 12 * 60_000 } },
};

/** A stream that failed: the error replaces the author. */
export const Failed: Story = {
  args: { state: { ...paused, error: "The book could not be streamed." } },
};

/** A title too long for its column: it is cut short. */
export const LongTitle: Story = {
  args: {
    state: {
      ...paused,
      target: {
        ...book,
        title: "The Remarkably Long and Winding Road Home Through the Northern Provinces",
      },
    },
  },
};
