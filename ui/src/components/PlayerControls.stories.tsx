/**
 * Stories for the player's controls: the transport buttons, the timeline
 * and the speed and sleep menus, in each state that looks different.
 */
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Player, type PlayerDeps, type PlayerState } from "@/lib/player";
import { Scrubber, SleepSelect, SpeedSelect, TransportButtons } from "./PlayerControls";

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
  loadSpeed: () => null,
  saveSpeed: () => {},
};

/** A book paused forty minutes into ten hours. */
const paused: PlayerState = {
  status: "paused",
  target: {
    editionId: "edition-1",
    workId: "work-1",
    title: "The Long Way Round",
    authors: "Avery Lindqvist",
    coverPath: null,
  },
  positionMs: 40 * 60_000,
  durationMs: 10 * 3_600_000,
  speed: 1,
  sleepAt: null,
  bookmarks: [],
  finished: false,
  error: null,
};

/**
 * An engine holding `state`, which the controls draw.
 *
 * @param state - The state to show.
 * @returns The engine.
 */
function playerIn(state: PlayerState): Player {
  const player = new Player(deps);
  player.state = state;
  return player;
}

/**
 * Every control over one state, as the now-playing view stacks them.
 *
 * @param props - The state to show, and `large` for the bigger buttons.
 */
function Controls({ state, large = false }: { state: PlayerState; large?: boolean }) {
  const player = playerIn(state);
  return (
    <div className="flex w-80 flex-col items-center gap-3 p-4">
      <Scrubber player={player} state={state} />
      <TransportButtons player={player} state={state} large={large} />
      <div className="flex items-center gap-2">
        <SpeedSelect player={player} state={state} />
        <SleepSelect player={player} state={state} />
      </div>
    </div>
  );
}

const meta = {
  title: "Player/PlayerControls",
  component: Controls,
  args: { state: paused },
} satisfies Meta<typeof Controls>;

export default meta;

/** A story of the player's controls. */
type Story = StoryObj<typeof meta>;

/** Paused part way through: the play button shows. */
export const Paused: Story = {};

/** Playing: the pause button shows. */
export const Playing: Story = { args: { state: { ...paused, status: "playing" } } };

/** Waiting for the stream: the middle button shows the throbber. */
export const Loading: Story = { args: { state: { ...paused, status: "loading" } } };

/** Length not yet known: the timeline is disabled. */
export const UnknownLength: Story = {
  args: { state: { ...paused, positionMs: 0, durationMs: null } },
};

/** Faster than normal, with a sleep timer twelve minutes from pausing. */
export const FastWithSleepTimer: Story = {
  args: { state: { ...paused, status: "playing", speed: 1.5, sleepAt: 12 * 60_000 } },
};

/** The larger buttons of the now-playing view. */
export const Large: Story = { args: { large: true } };
