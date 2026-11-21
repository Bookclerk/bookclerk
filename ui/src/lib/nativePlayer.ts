/**
 * The desktop window's own player, behind the web player's engine.
 *
 * The window's custom scheme cannot stream audio to the webview (it
 * carries no `Range`), so in the window the {@link Player} drives the
 * shell's native player instead of an audio element: {@link NativeAudio}
 * is an {@link AudioLike} over the shell's `player` command, and
 * {@link NativeMediaSession} is a {@link MediaSessionLike} that shows the
 * book in the system's media controls (MPRIS, the Windows media overlay,
 * Now Playing) and hears their buttons. Both hear the shell on one
 * channel, opened by the `player_events` command.
 *
 * @module
 */
import { listenOnChannel, type TauriBridge } from "@/lib/desktopLive";
import type { AudioLike } from "@/lib/player";
import type {
  MediaAction,
  MediaActionDetails,
  MediaDescription,
  MediaSessionLike,
} from "@/lib/mediaSession";

/** The shell command that runs one player command. */
export const PLAYER_COMMAND = "player";

/** The shell command that opens the player's event channel. */
export const PLAYER_EVENTS_COMMAND = "player_events";

/** What the shell's player says, as parsed from its JSON text. */
export type NativeEvent =
  | { type: "loaded"; durationMs: number | null; positionMs: number }
  | { type: "time"; positionMs: number }
  | { type: "playing" }
  | { type: "paused" }
  | { type: "ended" }
  | { type: "error"; message: string }
  | { type: "remote"; action: string; seekOffset?: number; seekTime?: number };

/**
 * Reads one message from the player's channel.
 *
 * @param message - The channel message (JSON text).
 * @returns The event, or `null` for anything malformed.
 */
export function parseNativeEvent(message: unknown): NativeEvent | null {
  if (typeof message !== "string") return null;
  let value: unknown;
  try {
    value = JSON.parse(message);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const body = value as Record<string, unknown>;
  const ms = (field: unknown): number | null =>
    typeof field === "number" && Number.isFinite(field) && field >= 0 ? field : null;
  switch (body.type) {
    case "loaded": {
      const positionMs = ms(body.positionMs);
      if (positionMs === null) return null;
      return { type: "loaded", durationMs: ms(body.durationMs), positionMs };
    }
    case "time": {
      const positionMs = ms(body.positionMs);
      return positionMs === null ? null : { type: "time", positionMs };
    }
    case "playing":
    case "paused":
    case "ended":
      return { type: body.type };
    case "error":
      return { type: "error", message: typeof body.message === "string" ? body.message : "" };
    case "remote": {
      if (typeof body.action !== "string") return null;
      const event: NativeEvent = { type: "remote", action: body.action };
      const offset = ms(body.seekOffset);
      const time = ms(body.seekTime);
      if (offset !== null) event.seekOffset = offset;
      if (time !== null) event.seekTime = time;
      return event;
    }
    default:
      return null;
  }
}

/** One command for the shell's player. */
export type NativeCommand =
  | { kind: "load"; track: string; positionMs: number }
  | { kind: "play" }
  | { kind: "pause" }
  | { kind: "seek"; positionMs: number }
  | { kind: "speed"; speed: number }
  | { kind: "stop" }
  | { kind: "probe"; track: string }
  | { kind: "describe"; title?: string; artist?: string }
  | { kind: "state"; state: "none" | "paused" | "playing" }
  | { kind: "position"; durationMs: number; positionMs: number; rate: number };

/**
 * The shell's player: sends commands and fans its events out.
 *
 * The event channel opens on first use; a listener added later hears
 * everything from then on.
 */
export class NativeBridge {
  /** Who hears the shell's events. */
  private readonly listeners = new Set<(event: NativeEvent) => void>();

  /** Whether the channel is open. */
  private listening = false;

  /**
   * A bridge over the page's Tauri bridge.
   *
   * @param bridge - The page's Tauri bridge.
   */
  constructor(bridge: TauriBridge) {
    this.bridge = bridge;
  }

  /** The page's Tauri bridge. */
  private readonly bridge: TauriBridge;

  /**
   * Hears the shell's events.
   *
   * @param listener - Called with each event.
   * @returns Stops hearing them.
   */
  listen(listener: (event: NativeEvent) => void): () => void {
    this.listeners.add(listener);
    this.open();
    return () => this.listeners.delete(listener);
  }

  /** Opens the event channel once. */
  private open(): void {
    if (this.listening) return;
    this.listening = true;
    const listening = listenOnChannel(this.bridge, (message) => {
      const event = parseNativeEvent(message);
      if (!event) return;
      for (const listener of [...this.listeners]) listener(event);
    });
    this.bridge
      .invoke(PLAYER_EVENTS_COMMAND, { onEvent: listening.channel })
      .catch((err: unknown) => {
        listening.close();
        this.listening = false;
        const message = err instanceof Error ? err.message : String(err);
        for (const listener of [...this.listeners]) listener({ type: "error", message });
      });
  }

  /**
   * Runs one command.
   *
   * @param command - The command.
   * @returns What the shell answered (a length for `probe`, else `null`).
   */
  async send(command: NativeCommand): Promise<unknown> {
    this.open();
    return this.bridge.invoke(PLAYER_COMMAND, { command });
  }
}

/** What {@link NativeAudio} raises, as an audio element names them. */
type MediaEventName = "loadedmetadata" | "timeupdate" | "play" | "pause" | "ended" | "error";

/**
 * An {@link AudioLike} over the shell's player, so the web player's
 * engine drives it as it would an audio element.
 *
 * Setting `src` loads the track paused at its start and raises
 * `loadedmetadata` once the shell has read it; setting `currentTime`
 * seeks. `paused`, `currentTime` and `duration` follow what the shell
 * reports. A command the shell refuses raises `error`, as a broken stream
 * would.
 */
export class NativeAudio implements AudioLike {
  /** The loaded track's stream path; empty when none. */
  private track = "";

  /** Seconds into the track, as last reported or set. */
  private seconds = 0;

  /** The track's length in seconds; `NaN` until known. */
  private length = Number.NaN;

  /** Whether not playing. */
  private stopped = true;

  /** The speed. */
  private rate = 1;

  /** Who hears each media event. */
  private readonly handlers = new Map<MediaEventName, (() => void)[]>();

  /**
   * An element over `bridge`.
   *
   * @param bridge - The shell's player.
   */
  constructor(bridge: NativeBridge) {
    this.bridge = bridge;
    bridge.listen((event) => this.hear(event));
  }

  /** The shell's player. */
  private readonly bridge: NativeBridge;

  /**
   * The loaded track's stream path.
   *
   * @returns The path; empty when none.
   */
  get src(): string {
    return this.track;
  }

  set src(path: string) {
    this.track = path;
    this.seconds = 0;
    this.length = Number.NaN;
    this.stopped = true;
    this.command(path ? { kind: "load", track: path, positionMs: 0 } : { kind: "stop" });
  }

  /**
   * Seconds from the start of the track.
   *
   * @returns The path.
   */
  get currentTime(): number {
    return this.seconds;
  }

  set currentTime(seconds: number) {
    const clamped = Math.max(0, Number.isFinite(seconds) ? seconds : 0);
    this.seconds = clamped;
    if (this.track) this.command({ kind: "seek", positionMs: Math.round(clamped * 1000) });
  }

  /**
   * The track's length in seconds; `NaN` until known.
   *
   * @returns Seconds.
   */
  get duration(): number {
    return this.length;
  }

  /**
   * True while not playing.
   *
   * @returns Whether paused.
   */
  get paused(): boolean {
    return this.stopped;
  }

  /**
   * The speed.
   *
   * @returns The speed.
   */
  get playbackRate(): number {
    return this.rate;
  }

  set playbackRate(speed: number) {
    if (speed === this.rate) return;
    this.rate = speed;
    if (this.track) this.command({ kind: "speed", speed });
  }

  /**
   * Starts playback.
   *
   * @returns Settles once the shell took the command.
   */
  async play(): Promise<void> {
    if (!this.track) return;
    await this.bridge.send({ kind: "play" });
  }

  /** Pauses playback. */
  pause(): void {
    if (!this.track) return;
    this.command({ kind: "pause" });
  }

  /**
   * Listens for a media event.
   *
   * @param type - The event's name.
   * @param listener - Called when it happens.
   */
  addEventListener(type: string, listener: () => void): void {
    const name = type as MediaEventName;
    this.handlers.set(name, [...(this.handlers.get(name) ?? []), listener]);
  }

  /**
   * Raises a media event.
   *
   * @param name - The event's name.
   */
  private raise(name: MediaEventName): void {
    for (const listener of this.handlers.get(name) ?? []) listener();
  }

  /**
   * Sends a command, raising `error` when the shell refuses it.
   *
   * @param command - The command.
   */
  private command(command: NativeCommand): void {
    this.bridge.send(command).catch(() => this.raise("error"));
  }

  /**
   * Follows what the shell reports.
   *
   * @param event - The shell's event.
   */
  private hear(event: NativeEvent): void {
    if (!this.track) return;
    switch (event.type) {
      case "loaded":
        this.length = event.durationMs === null ? Number.NaN : event.durationMs / 1000;
        this.seconds = event.positionMs / 1000;
        this.raise("loadedmetadata");
        break;
      case "time":
        this.seconds = event.positionMs / 1000;
        this.raise("timeupdate");
        break;
      case "playing":
        this.stopped = false;
        this.raise("play");
        break;
      case "paused":
        this.stopped = true;
        this.raise("pause");
        break;
      case "ended":
        this.stopped = true;
        this.raise("ended");
        break;
      case "error":
        this.stopped = true;
        this.raise("error");
        break;
      case "remote":
        break;
    }
  }
}

/**
 * A {@link MediaSessionLike} over the shell's player: what the page shows
 * the browser's Media Session goes to the system's media controls, and
 * their buttons come back to the page's handlers.
 *
 * `metadata` takes a {@link MediaDescription} (bind it with an identity
 * `describe`). The system's controls show no cover yet.
 */
export class NativeMediaSession implements MediaSessionLike {
  /** What is playing; `null` for nothing. */
  private shown: unknown = null;

  /** Whether it is playing. */
  private state: MediaSessionLike["playbackState"] = "none";

  /** The page's handlers. */
  private readonly actions = new Map<MediaAction, (details: MediaActionDetails) => void>();

  /**
   * A session over `bridge`.
   *
   * @param bridge - The shell's player.
   */
  constructor(bridge: NativeBridge) {
    this.bridge = bridge;
    bridge.listen((event) => {
      if (event.type === "remote") this.press(event);
    });
  }

  /** The shell's player. */
  private readonly bridge: NativeBridge;

  /**
   * What is playing.
   *
   * @returns The description last set.
   */
  get metadata(): unknown {
    return this.shown;
  }

  set metadata(value: unknown) {
    this.shown = value;
    const description = value as Partial<MediaDescription> | null;
    const command: NativeCommand =
      description && typeof description.title === "string"
        ? {
            kind: "describe",
            title: description.title,
            ...(description.artist ? { artist: description.artist } : {}),
          }
        : { kind: "describe" };
    this.send(command);
  }

  /**
   * Whether it is playing.
   *
   * @returns The state last set.
   */
  get playbackState(): MediaSessionLike["playbackState"] {
    return this.state;
  }

  set playbackState(state: MediaSessionLike["playbackState"]) {
    if (state === this.state) return;
    this.state = state;
    this.send({ kind: "state", state });
  }

  /**
   * Registers or clears an action's handler.
   *
   * @param action - The action.
   * @param handler - Its handler; `null` clears it.
   */
  setActionHandler(
    action: MediaAction,
    handler: ((details: MediaActionDetails) => void) | null,
  ): void {
    if (handler) this.actions.set(action, handler);
    else this.actions.delete(action);
  }

  /**
   * Tells the system's controls the position, for their own scrubber.
   *
   * @param state - Seconds and speed; omitted clears it.
   */
  setPositionState(state?: { duration: number; playbackRate: number; position: number }): void {
    if (!state) return;
    this.send({
      kind: "position",
      durationMs: Math.round(state.duration * 1000),
      positionMs: Math.round(state.position * 1000),
      rate: state.playbackRate,
    });
  }

  /**
   * Runs the handler for a button pressed in the system's controls.
   *
   * @param event - The remote event.
   */
  private press(event: Extract<NativeEvent, { type: "remote" }>): void {
    const action =
      event.action === "toggle" ? (this.state === "playing" ? "pause" : "play") : event.action;
    const handler = this.actions.get(action as MediaAction);
    handler?.({ seekOffset: event.seekOffset ?? null, seekTime: event.seekTime ?? null });
  }

  /**
   * Sends a command; the controls are a convenience, so a refusal is dropped.
   *
   * @param command - The command.
   */
  private send(command: NativeCommand): void {
    this.bridge.send(command).catch(() => undefined);
  }
}

/**
 * Reads a track's length through the shell.
 *
 * @param bridge - The shell's player.
 * @param path - The track's stream path.
 * @returns Its length in milliseconds.
 */
export async function nativeProbe(bridge: NativeBridge, path: string): Promise<number> {
  const ms = await bridge.send({ kind: "probe", track: path });
  if (typeof ms !== "number" || !Number.isFinite(ms)) throw new Error("unknown length");
  return ms;
}
