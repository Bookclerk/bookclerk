import { describe, expect, it } from "vitest";
import {
  MEDIA_ACTIONS,
  bindMediaSession,
  type MediaAction,
  type MediaActionDetails,
  type MediaDescription,
  type MediaSessionLike,
} from "./mediaSession";
import { SKIP_MS, type Player, type PlayerState } from "./player";

// A player stand-in that records what the session asked of it.
function fakePlayer() {
  const calls: string[] = [];
  const listeners = new Set<() => void>();
  const state: PlayerState = {
    status: "idle",
    target: null,
    positionMs: 0,
    durationMs: null,
    speed: 1,
    sleepAt: null,
    bookmarks: [],
    finished: false,
    error: null,
  };
  const player = {
    state,
    subscribe: (listener: () => void) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    resume: () => calls.push("resume"),
    pause: () => calls.push("pause"),
    stop: async () => {
      calls.push("stop");
    },
    skip: (ms: number) => calls.push(`skip ${ms}`),
    seek: (ms: number) => calls.push(`seek ${ms}`),
  };
  const set = (patch: Partial<PlayerState>) => {
    player.state = { ...player.state, ...patch };
    for (const listener of listeners) listener();
  };
  return { player: player as unknown as Player, calls, set, listeners };
}

function fakeSession(refuse: MediaAction[] = []) {
  const handlers = new Map<MediaAction, ((details: MediaActionDetails) => void) | null>();
  const positions: ({ duration: number; playbackRate: number; position: number } | undefined)[] =
    [];
  const session: MediaSessionLike = {
    metadata: null,
    playbackState: "none",
    setActionHandler: (action, handler) => {
      if (refuse.includes(action)) throw new TypeError("unsupported");
      handlers.set(action, handler);
    },
    setPositionState: (state) => positions.push(state),
  };
  return { session, handlers, positions };
}

const describeBook = (description: MediaDescription) => description;
const covers = (path: string | null) => (path ? [{ src: `${path}/360`, sizes: "360x360" }] : []);
const TARGET = {
  editionId: "ed1",
  workId: "w1",
  title: "Piranesi",
  authors: "Susanna Clarke",
  coverPath: "/api/covers/abc",
};

describe("bindMediaSession", () => {
  it("does nothing where the browser has no media session", () => {
    const { player, listeners } = fakePlayer();
    bindMediaSession(player, undefined, describeBook, covers)();
    expect(listeners.size).toBe(0);
  });

  it("routes the OS controls to the player", () => {
    const { player, calls } = fakePlayer();
    const { session, handlers } = fakeSession();
    bindMediaSession(player, session, describeBook, covers);
    expect([...handlers.keys()]).toEqual([...MEDIA_ACTIONS]);
    handlers.get("play")?.({});
    handlers.get("pause")?.({});
    handlers.get("seekbackward")?.({});
    handlers.get("seekforward")?.({ seekOffset: 10 });
    handlers.get("seekto")?.({ seekTime: 61.5 });
    handlers.get("seekto")?.({});
    handlers.get("previoustrack")?.({});
    handlers.get("nexttrack")?.({});
    handlers.get("stop")?.({});
    expect(calls).toEqual([
      "resume",
      "pause",
      `skip ${-SKIP_MS}`,
      "skip 10000",
      "seek 61500",
      `skip ${-SKIP_MS}`,
      `skip ${SKIP_MS}`,
      "stop",
    ]);
  });

  it("shows the book, its state and its position", () => {
    const { player, set } = fakePlayer();
    const { session, positions } = fakeSession();
    bindMediaSession(player, session, describeBook, covers);
    expect(session.metadata).toBeNull();
    expect(positions).toEqual([undefined]);

    set({ target: TARGET, status: "loading" });
    expect(session.metadata).toEqual({
      title: "Piranesi",
      artist: "Susanna Clarke",
      artwork: [{ src: "/api/covers/abc/360", sizes: "360x360" }],
    });
    expect(session.playbackState).toBe("paused");

    set({ status: "playing", durationMs: 600_000, positionMs: 30_000 });
    expect(session.playbackState).toBe("playing");
    expect(positions.at(-1)).toEqual({ duration: 600, playbackRate: 1, position: 30 });

    // Ordinary progress is the OS's own clock's to count.
    const sent = positions.length;
    set({ positionMs: 30_500 });
    set({ positionMs: 31_000 });
    expect(positions).toHaveLength(sent);
    set({ positionMs: 90_000 });
    expect(positions.at(-1)?.position).toBe(90);
    set({ speed: 1.5 });
    expect(positions.at(-1)?.playbackRate).toBe(1.5);

    set({ target: { ...TARGET, authors: null, coverPath: null } });
    expect(session.metadata).toMatchObject({ artist: "", artwork: [] });
    set({ target: null, status: "idle", durationMs: null, positionMs: 0 });
    expect(session.metadata).toBeNull();
    expect(session.playbackState).toBe("none");
    expect(positions.at(-1)).toBeUndefined();
  });

  it("survives a browser that refuses an action or a position", () => {
    const { player, set } = fakePlayer();
    const { session, handlers } = fakeSession(["seekto", "stop"]);
    session.setPositionState = () => {
      throw new TypeError("position past duration");
    };
    const unbind = bindMediaSession(player, session, describeBook, covers);
    expect(handlers.has("seekto")).toBe(false);
    expect(handlers.has("play")).toBe(true);
    set({ target: TARGET, status: "playing", durationMs: 1000, positionMs: 500 });
    unbind();
    expect(handlers.get("play")).toBeNull();
  });

  it("clears the session when unbound", () => {
    const { player, set, listeners } = fakePlayer();
    const { session, handlers } = fakeSession();
    session.setPositionState = undefined;
    const unbind = bindMediaSession(player, session, describeBook, covers);
    set({ target: TARGET, status: "playing" });
    unbind();
    expect(listeners.size).toBe(0);
    expect(session.metadata).toBeNull();
    expect(session.playbackState).toBe("none");
    expect([...handlers.values()].every((handler) => handler === null)).toBe(true);
  });
});
