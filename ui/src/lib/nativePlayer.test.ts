import { describe, expect, it, vi } from "vitest";
import type { TauriBridge } from "@/lib/desktopLive";
import {
  NativeAudio,
  NativeBridge,
  NativeMediaSession,
  PLAYER_COMMAND,
  PLAYER_EVENTS_COMMAND,
  nativeProbe,
  parseNativeEvent,
} from "@/lib/nativePlayer";

/**
 * A shell that records commands and lets the test send player events.
 *
 * @param answer - How the `player` command answers.
 * @returns The bridge, the commands sent, and a way to emit events.
 */
function fakeShell(answer: (command: unknown) => Promise<unknown> = async () => null) {
  const callbacks = new Map<number, (delivery: unknown) => void>();
  const commands: unknown[] = [];
  let nextId = 1;
  let index = 0;
  const bridge: TauriBridge = {
    invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
      if (command === PLAYER_COMMAND) {
        commands.push(args?.command);
        return answer(args?.command);
      }
      return null;
    }),
    transformCallback: (callback) => {
      const id = nextId++;
      callbacks.set(id, callback);
      return id;
    },
    unregisterCallback: (id) => {
      callbacks.delete(id);
    },
  };
  const emit = (event: object) => {
    const delivery = { index: index++, message: JSON.stringify(event) };
    for (const callback of callbacks.values()) callback(delivery);
  };
  return { bridge, commands, emit };
}

/**
 * Lets pending promise callbacks run.
 *
 * @returns Settles on the next macrotask.
 */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("parseNativeEvent", () => {
  it("reads the shell's events and refuses anything else", () => {
    expect(parseNativeEvent('{"type":"loaded","durationMs":null,"positionMs":3}')).toEqual({
      type: "loaded",
      durationMs: null,
      positionMs: 3,
    });
    expect(parseNativeEvent('{"type":"time","positionMs":-1}')).toBeNull();
    expect(parseNativeEvent('{"type":"remote","action":"seekto","seekTime":1.5}')).toEqual({
      type: "remote",
      action: "seekto",
      seekTime: 1.5,
    });
    expect(parseNativeEvent('{"type":"launch"}')).toBeNull();
    expect(parseNativeEvent("not json")).toBeNull();
    expect(parseNativeEvent(5)).toBeNull();
  });
});

describe("NativeAudio", () => {
  it("opens the event channel once and loads a track paused", async () => {
    const { bridge, commands } = fakeShell();
    const native = new NativeBridge(bridge);
    const audio = new NativeAudio(native);
    audio.src = "/api/playback/sessions/s/tracks/0";
    audio.playbackRate = 1.5;
    await settle();
    const events = vi
      .mocked(bridge.invoke)
      .mock.calls.filter(([command]) => command === PLAYER_EVENTS_COMMAND);
    expect(events).toHaveLength(1);
    expect(commands).toEqual([
      { kind: "load", track: "/api/playback/sessions/s/tracks/0", positionMs: 0 },
      { kind: "speed", speed: 1.5 },
    ]);
    expect(audio.paused).toBe(true);
    expect(Number.isNaN(audio.duration)).toBe(true);
  });

  it("raises media events as the shell reports", async () => {
    const { bridge, commands, emit } = fakeShell();
    const audio = new NativeAudio(new NativeBridge(bridge));
    const seen: string[] = [];
    for (const name of ["loadedmetadata", "timeupdate", "play", "pause", "ended"]) {
      audio.addEventListener(name, () => seen.push(name));
    }
    audio.src = "/t";
    emit({ type: "loaded", durationMs: 90_000, positionMs: 0 });
    expect(audio.duration).toBe(90);
    audio.currentTime = 30;
    await audio.play();
    emit({ type: "playing" });
    expect(audio.paused).toBe(false);
    emit({ type: "time", positionMs: 31_000 });
    expect(audio.currentTime).toBe(31);
    emit({ type: "ended" });
    expect(audio.paused).toBe(true);
    expect(seen).toEqual(["loadedmetadata", "play", "timeupdate", "ended"]);
    expect(commands).toContainEqual({ kind: "seek", positionMs: 30_000 });
    expect(commands).toContainEqual({ kind: "play" });
  });

  it("raises error when the shell refuses a command or the track fails", async () => {
    const { bridge, emit } = fakeShell(async () => {
      throw new Error("no audio output device");
    });
    const audio = new NativeAudio(new NativeBridge(bridge));
    let errors = 0;
    audio.addEventListener("error", () => (errors += 1));
    audio.src = "/t";
    await settle();
    expect(errors).toBe(1);
    emit({ type: "error", message: "the play session ended" });
    expect(errors).toBe(2);
  });

  it("ignores events with nothing loaded and stops on an empty source", async () => {
    const { bridge, commands, emit } = fakeShell();
    const audio = new NativeAudio(new NativeBridge(bridge));
    let loaded = 0;
    audio.addEventListener("loadedmetadata", () => (loaded += 1));
    emit({ type: "loaded", durationMs: 1, positionMs: 0 });
    audio.pause();
    expect(loaded).toBe(0);
    audio.src = "/t";
    audio.src = "";
    await settle();
    expect(commands).toEqual([{ kind: "load", track: "/t", positionMs: 0 }, { kind: "stop" }]);
  });
});

describe("NativeMediaSession", () => {
  it("shows the description, state and position in the system's controls", async () => {
    const { bridge, commands } = fakeShell();
    const session = new NativeMediaSession(new NativeBridge(bridge));
    session.metadata = { title: "Dune", artist: "Frank Herbert", artwork: [] };
    session.playbackState = "playing";
    session.playbackState = "playing";
    session.setPositionState({ duration: 60, playbackRate: 1.25, position: 2.5 });
    session.setPositionState();
    session.metadata = null;
    await settle();
    expect(commands).toEqual([
      { kind: "describe", title: "Dune", artist: "Frank Herbert" },
      { kind: "state", state: "playing" },
      { kind: "position", durationMs: 60_000, positionMs: 2_500, rate: 1.25 },
      { kind: "describe" },
    ]);
  });

  it("routes the controls' buttons to the page's handlers", () => {
    const { bridge, emit } = fakeShell();
    const session = new NativeMediaSession(new NativeBridge(bridge));
    const pressed: string[] = [];
    session.setActionHandler("play", () => pressed.push("play"));
    session.setActionHandler("pause", () => pressed.push("pause"));
    session.setActionHandler("seekbackward", (details) =>
      pressed.push(`back ${details.seekOffset}`),
    );
    session.setActionHandler("seekto", (details) => pressed.push(`to ${details.seekTime}`));
    emit({ type: "remote", action: "toggle" });
    session.playbackState = "playing";
    emit({ type: "remote", action: "toggle" });
    emit({ type: "remote", action: "seekbackward", seekOffset: 10 });
    emit({ type: "remote", action: "seekto", seekTime: 4 });
    session.setActionHandler("play", null);
    emit({ type: "remote", action: "play" });
    expect(pressed).toEqual(["play", "pause", "back 10", "to 4"]);
  });
});

describe("nativeProbe", () => {
  it("answers the shell's length and refuses an unknown one", async () => {
    const lengths = [1234, null];
    const { bridge } = fakeShell(async () => lengths.shift());
    const native = new NativeBridge(bridge);
    await expect(nativeProbe(native, "/t")).resolves.toBe(1234);
    await expect(nativeProbe(native, "/t")).rejects.toThrow("unknown length");
  });
});
