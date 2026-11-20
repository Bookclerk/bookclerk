/**
 * The web player's engine: one audio element, one play session, and the
 * reports that keep the library's progress current.
 *
 * The engine knows nothing of React. Components read {@link Player.state}
 * through {@link Player.subscribe}; tests drive it with a fake audio
 * element and fake routes.
 *
 * @module
 */
import { isApiError } from "@/lib/api";
import type {
  Bookmark,
  PlaybackManifest,
  PlaybackReport,
  PlaybackSession,
  PlaybackSynced,
} from "@/lib/playback";
import { locate, nearestSpeed, trackStarts } from "@/lib/playerTimeline";

/** What the engine needs of an audio element (an `HTMLAudioElement` has it). */
export interface AudioLike {
  /** The stream URL. */
  src: string;
  /** Seconds from the start of the loaded file. */
  currentTime: number;
  /** The loaded file's length in seconds; `NaN` until known. */
  readonly duration: number;
  /** True while not playing. */
  readonly paused: boolean;
  /** Playback speed. */
  playbackRate: number;
  /** Starts playback. */
  play(): Promise<void>;
  /** Pauses playback. */
  pause(): void;
  /** Listens for a media event. */
  addEventListener(type: string, listener: () => void): void;
}

/** The routes and browser facilities the engine uses. */
export interface PlayerDeps {
  /** Makes the audio element. */
  audio(): AudioLike;
  /** `GET .../playback`. */
  manifest(editionId: string): Promise<PlaybackManifest>;
  /** `POST /api/playback/sessions`. */
  open(editionId: string): Promise<PlaybackSession>;
  /** `POST .../sync`. */
  sync(sessionId: string, report: PlaybackReport): Promise<PlaybackSynced>;
  /** `POST .../close`. */
  close(sessionId: string, report: PlaybackReport, keepalive: boolean): Promise<PlaybackSynced>;
  /** `POST .../bookmarks`. */
  addBookmark(editionId: string, positionMs: number, title: string): Promise<Bookmark>;
  /** `DELETE /api/playback/bookmarks/{id}`. */
  deleteBookmark(id: string): Promise<void>;
  /** `PUT /api/playback/progress/{id}`. */
  setProgress(editionId: string, edit: { is_finished: boolean }): Promise<PlaybackSynced>;
  /** A track's length in milliseconds, read from its stream. */
  probe(path: string): Promise<number>;
  /** Monotonic milliseconds. */
  now(): number;
  /** Reads the stored speed; `null` when none. */
  loadSpeed(): string | null;
  /** Stores the speed. */
  saveSpeed(speed: number): void;
}

/** What is being played, as the caller knows it. */
export interface PlayTarget {
  /** The edition to play. */
  editionId: string;
  /** Its work, for the cover and a link back. */
  workId: string | null;
  /** Display title. */
  title: string;
  /** Authors, when known. */
  authors: string | null;
  /** Same-origin cover path (`cover_path`), when known. */
  coverPath: string | null;
}

/** The engine's state, replaced (never mutated) on every change. */
export interface PlayerState {
  /** `idle` with nothing loaded; `error` keeps the target and a message. */
  status: "idle" | "loading" | "playing" | "paused" | "error";
  /** What is loaded. */
  target: PlayTarget | null;
  /** Position on the edition's timeline. */
  positionMs: number;
  /** The edition's runtime; `null` until known. */
  durationMs: number | null;
  /** Playback speed. */
  speed: number;
  /** When the sleep timer pauses playback ({@link PlayerDeps.now} time). */
  sleepAt: number | null;
  /** The listener's bookmarks in the loaded edition, in timeline order. */
  bookmarks: Bookmark[];
  /** True once the listener reached the end in this sitting. */
  finished: boolean;
  /** What went wrong, in words for the listener. */
  error: string | null;
}

/** Milliseconds between position reports while playing. */
export const SYNC_EVERY_MS = 15_000;

/** Milliseconds before a second stream failure earns another new session. */
const RETRY_AFTER_MS = 60_000;

/** Milliseconds one skip button moves. */
export const SKIP_MS = 30_000;

/**
 * The state with nothing loaded.
 *
 * @param speed - The speed to keep.
 * @returns The idle state.
 */
function idle(speed: number): PlayerState {
  return {
    status: "idle",
    target: null,
    positionMs: 0,
    durationMs: null,
    speed,
    sleepAt: null,
    bookmarks: [],
    finished: false,
    error: null,
  };
}

/**
 * Words for a failure to start or continue playing.
 *
 * @param err - What a route or the audio element threw.
 * @returns A sentence for the listener.
 */
export function playbackErrorText(err: unknown): string {
  if (isApiError(err)) {
    if (err.code === "not_playable") return "This book cannot be played from this server.";
    if (err.status === 403) return "Sign in as a library user to listen.";
    if (err.status === 404) return "This book is no longer in your library.";
    if (err.status === 421) return "This book is playing from another server. Start it again.";
    if (err.status === 503) return "The server cannot reach its storage right now.";
  }
  if (err instanceof DOMException && err.name === "NotAllowedError") {
    return "The browser blocked playback. Press play again.";
  }
  return "This book could not be played.";
}

/** The web player's engine. One per page. */
export class Player {
  /** Current state. */
  state: PlayerState;
  /** Routes and browser facilities. */
  private readonly deps: PlayerDeps;
  /** The audio element; made on first play. */
  private audio: AudioLike | null = null;
  /** State listeners. */
  private readonly listeners = new Set<() => void>();
  /** The open session. */
  private session: PlaybackSession | null = null;
  /** Each track's length in milliseconds. */
  private durations: number[] = [];
  /** The loaded track's index. */
  private track = 0;
  /** Where to seek once the loaded track's metadata arrives. */
  private pendingSeekMs: number | null = null;
  /** True when the loaded track should play as soon as it can. */
  private wantPlay = false;
  /** Milliseconds listened since the last report. */
  private listened = 0;
  /** {@link PlayerDeps.now} at the last listened-time tick; `null` while paused. */
  private tickedAt: number | null = null;
  /** {@link PlayerDeps.now} at the last report. */
  private syncedAt = 0;
  /** True while a report is in flight. */
  private syncing = false;
  /**
   * The session the report in flight is for and the listened milliseconds it
   * carries; `null` with none in flight.
   */
  private sending: { session: PlaybackSession; listenedMs: number } | null = null;
  /** True when a report was asked for while another was in flight. */
  private reportAgain = false;
  /** Counts loads, so an answer for an earlier one is dropped. */
  private generation = 0;
  /** {@link PlayerDeps.now} at the last automatic new session for a stream failure. */
  private retriedAt: number | null = null;

  /**
   * @param deps - Routes and browser facilities.
   */
  constructor(deps: PlayerDeps) {
    this.deps = deps;
    this.state = idle(nearestSpeed(deps.loadSpeed() ?? 1));
  }

  /**
   * Counts every load and unload. A caller that reads something before
   * asking to play compares this before and after, and drops its request
   * when the listener has started or stopped a book meanwhile.
   *
   * @returns The count so far.
   */
  get loads(): number {
    return this.generation;
  }

  /**
   * Listens for state changes.
   *
   * @param listener - Called after each change.
   * @returns Stops listening.
   */
  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  /**
   * Replaces part of the state and tells the listeners.
   *
   * @param patch - The fields to change.
   */
  private set(patch: Partial<PlayerState>): void {
    this.state = { ...this.state, ...patch };
    for (const listener of this.listeners) listener();
  }

  /**
   * The audio element, made and wired on first use.
   *
   * @returns The element.
   */
  private element(): AudioLike {
    if (this.audio) return this.audio;
    const audio = this.deps.audio();
    audio.addEventListener("loadedmetadata", () => this.onMetadata());
    audio.addEventListener("timeupdate", () => this.onTime());
    audio.addEventListener("play", () => this.onPlaying());
    audio.addEventListener("pause", () => this.onPaused());
    audio.addEventListener("ended", () => void this.onEnded());
    audio.addEventListener("error", () => void this.onStreamError());
    this.audio = audio;
    return audio;
  }

  /**
   * Plays an edition from where the listener left it, closing whatever
   * played before. Asking for the loaded edition resumes it.
   *
   * @param target - What to play.
   */
  async play(target: PlayTarget): Promise<void> {
    if (this.state.target?.editionId === target.editionId && this.session) {
      this.resume();
      return;
    }
    // Everything that decides which request is the latest happens before
    // the first await, so a slow close of the book before cannot let an
    // earlier request overtake a later one.
    const closing = this.stop();
    const generation = (this.generation += 1);
    this.retriedAt = null;
    this.wantPlay = true;
    this.set({ ...idle(this.state.speed), status: "loading", target });
    await closing;
    if (generation !== this.generation) return;
    try {
      const session = await this.deps.open(target.editionId);
      if (generation !== this.generation) {
        void this.deps.close(session.session_id, { position_ms: session.position_ms }, false);
        return;
      }
      await this.adopt(session, session.position_ms, generation);
      if (generation !== this.generation) return;
      // Bookmarks are a convenience: playing does not wait on them.
      void this.deps
        .manifest(target.editionId)
        .then((manifest) => {
          if (generation === this.generation) this.set({ bookmarks: manifest.bookmarks });
        })
        .catch(() => undefined);
    } catch (err) {
      if (generation === this.generation) {
        this.session = null;
        this.set({ status: "error", error: playbackErrorText(err) });
      }
    }
  }

  /**
   * Takes a session's tracks and loads the one `positionMs` falls in.
   *
   * @param session - The session just opened.
   * @param positionMs - Where to start on the timeline.
   * @param generation - The load this answer belongs to.
   */
  private async adopt(
    session: PlaybackSession,
    positionMs: number,
    generation: number,
  ): Promise<void> {
    const paths = session.tracks.map((track) => track.path ?? "");
    if (paths.length === 0 || paths.some((path) => !path)) {
      throw new Error("the session lists no tracks");
    }
    // One file is its own timeline. Several need every length before a
    // position can be placed in one of them.
    const durations =
      paths.length === 1
        ? [Number.POSITIVE_INFINITY]
        : await Promise.all(paths.map((path) => this.deps.probe(path)));
    if (generation !== this.generation) {
      void this.deps.close(session.session_id, { position_ms: positionMs }, false);
      return;
    }
    this.session = session;
    this.durations = durations;
    this.listened = 0;
    this.syncedAt = this.deps.now();
    const total = paths.length > 1 ? durations.reduce((sum, ms) => sum + ms, 0) : null;
    // A listener who pressed pause while the session opened gets a book
    // that is loaded and waiting, not one that starts anyway.
    this.set({
      positionMs,
      durationMs: total ?? session.duration_ms,
      status: !this.wantPlay && this.state.status === "loading" ? "paused" : this.state.status,
    });
    this.load(locate(durations, positionMs).index, positionMs);
  }

  /**
   * Points the audio element at a track and seeks once it is ready.
   *
   * @param index - The track.
   * @param positionMs - The timeline position to seek to.
   */
  private load(index: number, positionMs: number): void {
    const audio = this.element();
    this.track = index;
    this.pendingSeekMs = positionMs;
    audio.src = this.session?.tracks[index]?.path ?? "";
    audio.playbackRate = this.state.speed;
  }

  /** The loaded track's metadata arrived: seek, then play if wanted. */
  private onMetadata(): void {
    const audio = this.audio;
    if (!audio || !this.session) return;
    if (this.durations.length === 1 && Number.isFinite(audio.duration)) {
      this.durations = [audio.duration * 1000];
      this.set({ durationMs: Math.round(audio.duration * 1000) });
    }
    if (this.pendingSeekMs !== null) {
      const place = locate(this.durations, this.pendingSeekMs);
      audio.currentTime = place.index === this.track ? place.offsetMs / 1000 : 0;
      this.pendingSeekMs = null;
    }
    audio.playbackRate = this.state.speed;
    if (this.wantPlay) this.start();
  }

  /** Starts the audio element, reporting a refusal. */
  private start(): void {
    const audio = this.audio;
    if (!audio) return;
    audio.play().catch((err: unknown) => {
      // A pause or a new source interrupts a pending play; that is not a
      // failure.
      if (err instanceof DOMException && err.name === "AbortError") return;
      this.wantPlay = false;
      this.set({ status: "paused", error: playbackErrorText(err) });
    });
  }

  /**
   * The position the audio element is at, on the whole timeline.
   *
   * @returns Milliseconds from the edition's start.
   */
  private position(): number {
    const audio = this.audio;
    if (!audio || this.pendingSeekMs !== null) {
      return this.pendingSeekMs ?? this.state.positionMs;
    }
    return Math.round(trackStarts(this.durations)[this.track] + audio.currentTime * 1000);
  }

  /** Adds the time since the last tick to the listened total. */
  private tick(): void {
    const now = this.deps.now();
    if (this.tickedAt !== null) {
      // A long gap is a suspended tab, not listening.
      this.listened += Math.min(Math.max(0, now - this.tickedAt), 2000);
    }
    this.tickedAt = this.audio && !this.audio.paused ? now : null;
  }

  /** The audio element moved. */
  private onTime(): void {
    if (!this.session) return;
    this.tick();
    this.set({ positionMs: this.position() });
    const now = this.deps.now();
    if (this.state.sleepAt !== null && now >= this.state.sleepAt) {
      this.set({ sleepAt: null });
      this.pause();
      return;
    }
    if (now - this.syncedAt >= SYNC_EVERY_MS) void this.report();
  }

  /** The audio element started. */
  private onPlaying(): void {
    if (!this.session) return;
    this.tickedAt = this.deps.now();
    this.set({ status: "playing", error: null });
  }

  /** The audio element stopped. */
  private onPaused(): void {
    if (!this.session) return;
    this.tick();
    this.tickedAt = null;
    if (this.state.status === "playing") {
      this.set({ status: "paused", positionMs: this.position() });
      void this.report();
    }
  }

  /** A track played to its end: the next one, or the end of the edition. */
  private async onEnded(): Promise<void> {
    if (!this.session) return;
    this.tick();
    if (this.track + 1 < this.durations.length) {
      this.wantPlay = true;
      this.load(this.track + 1, trackStarts(this.durations)[this.track + 1]);
      return;
    }
    this.wantPlay = false;
    this.set({ status: "paused", positionMs: this.state.durationMs ?? this.position() });
    await this.report();
  }

  /**
   * The stream failed. A session that ended (a day idle, changed files) is
   * the usual cause, so open one new session at the same place.
   */
  private async onStreamError(): Promise<void> {
    if (!this.session || !this.state.target) return;
    const now = this.deps.now();
    // A stream that fails again right after a new session is broken, not ended.
    if (this.retriedAt !== null && now - this.retriedAt < RETRY_AFTER_MS) {
      this.wantPlay = false;
      this.set({ status: "error", error: playbackErrorText(null) });
      return;
    }
    this.retriedAt = now;
    await this.reopen();
  }

  /** Replaces the session with a new one at the current position. */
  private async reopen(): Promise<void> {
    const target = this.state.target;
    if (!target) return;
    const generation = this.generation;
    const positionMs = this.state.positionMs;
    // Decided before the await, so a pause pressed meanwhile still holds.
    this.wantPlay = this.wantPlay || this.state.status === "playing";
    try {
      const session = await this.deps.open(target.editionId);
      if (generation !== this.generation) {
        void this.deps.close(session.session_id, { position_ms: positionMs }, false);
        return;
      }
      await this.adopt(session, positionMs, generation);
    } catch (err) {
      if (generation === this.generation) {
        // With no session nothing reports or stops what is buffered, so
        // the audio goes with it.
        this.session = null;
        this.wantPlay = false;
        this.tickedAt = null;
        this.audio?.pause();
        if (this.audio) this.audio.src = "";
        this.set({ status: "error", error: playbackErrorText(err) });
      }
    }
  }

  /**
   * The report for the current position.
   *
   * @param session - The session the report is for.
   * @returns The body of a sync or close request.
   */
  private reportBody(session: PlaybackSession): PlaybackReport {
    // What a report in flight for this session already carries is not sent
    // twice; one in flight for an earlier session carried none of this time.
    const inFlight = this.sending?.session === session ? this.sending.listenedMs : 0;
    const body: PlaybackReport = {
      position_ms: Math.max(0, Math.round(this.state.positionMs)),
      listened_ms: Math.round(Math.max(0, this.listened - inFlight)),
    };
    if (this.state.durationMs) body.duration_ms = Math.round(this.state.durationMs);
    return body;
  }

  /**
   * Sends a report; a session the server ended is replaced. A report asked
   * for while one is in flight (a pause, the end of the book) follows it
   * with the position as it then is.
   */
  private async report(): Promise<void> {
    const session = this.session;
    if (!session) return;
    if (this.syncing) {
      this.reportAgain = true;
      return;
    }
    this.syncing = true;
    this.syncedAt = this.deps.now();
    const body = this.reportBody(session);
    const sent = this.listened;
    this.sending = { session, listenedMs: sent };
    try {
      const synced = await this.deps.sync(session.session_id, body);
      // An answer for a session since replaced says nothing about this one.
      if (this.session === session) {
        this.listened = Math.max(0, this.listened - sent);
        if (synced.finished_now) this.set({ finished: true });
      }
    } catch (err) {
      if (isApiError(err) && err.status === 409 && this.session === session) {
        await this.reopen();
      }
      // Anything else is retried by the next report.
    } finally {
      this.syncing = false;
      this.sending = null;
    }
    if (this.reportAgain) {
      this.reportAgain = false;
      await this.report();
    }
  }

  /** Resumes the loaded edition. */
  resume(): void {
    if (!this.session) {
      // Still opening: play once it has. Otherwise open it again.
      if (this.state.status === "loading") this.wantPlay = true;
      else if (this.state.target) void this.play(this.state.target);
      return;
    }
    const duration = this.state.durationMs;
    if (duration && this.state.positionMs >= duration) this.seek(0);
    this.wantPlay = true;
    if (this.pendingSeekMs === null) {
      this.set({ error: null });
      this.start();
    } else {
      // The file is not ready yet: it plays when its metadata arrives.
      this.set({ error: null, status: "loading" });
    }
  }

  /**
   * Pauses playback. A book still loading stays loaded and does not start.
   */
  pause(): void {
    this.wantPlay = false;
    this.audio?.pause();
    if (this.session && this.state.status === "loading") this.set({ status: "paused" });
  }

  /** Plays when paused, pauses when playing or about to play. */
  toggle(): void {
    const starting = this.state.status === "loading" && this.wantPlay;
    if (this.state.status === "playing" || starting) this.pause();
    else this.resume();
  }

  /**
   * Moves to a position on the edition's timeline.
   *
   * @param positionMs - The position; clamped to the runtime.
   */
  seek(positionMs: number): void {
    const audio = this.audio;
    if (!this.session || !audio) return;
    const end = this.state.durationMs ?? Number.POSITIVE_INFINITY;
    const position = Math.min(Math.max(0, positionMs), end);
    const place = locate(this.durations, position);
    if (place.index !== this.track) {
      this.wantPlay = this.wantPlay || this.state.status === "playing";
      this.load(place.index, position);
    } else if (this.pendingSeekMs !== null) {
      this.pendingSeekMs = position;
    } else {
      audio.currentTime = place.offsetMs / 1000;
    }
    this.set({ positionMs: Math.round(position), finished: false });
  }

  /**
   * Moves by an amount.
   *
   * @param deltaMs - Milliseconds forward (negative for back).
   */
  skip(deltaMs: number): void {
    this.seek(this.state.positionMs + deltaMs);
  }

  /**
   * Changes the playback speed and remembers it.
   *
   * @param speed - The speed; snapped to an offered one.
   */
  setSpeed(speed: number): void {
    const chosen = nearestSpeed(speed);
    if (this.audio) this.audio.playbackRate = chosen;
    this.deps.saveSpeed(chosen);
    this.set({ speed: chosen });
  }

  /**
   * Sets or clears the sleep timer.
   *
   * @param minutes - Minutes until playback pauses; `null` clears it.
   */
  setSleep(minutes: number | null): void {
    this.set({ sleepAt: minutes === null ? null : this.deps.now() + minutes * 60_000 });
  }

  /**
   * How long until the sleep timer pauses playback.
   *
   * @returns Milliseconds left; `null` when no timer is set.
   */
  sleepRemainingMs(): number | null {
    const at = this.state.sleepAt;
    return at === null ? null : Math.max(0, at - this.deps.now());
  }

  /**
   * Marks the current place.
   *
   * @param title - A note; may be empty.
   */
  async addBookmark(title: string): Promise<void> {
    const target = this.state.target;
    if (!target) return;
    const bookmark = await this.deps.addBookmark(target.editionId, this.state.positionMs, title);
    if (this.state.target?.editionId !== target.editionId) return;
    this.set({
      bookmarks: [...this.state.bookmarks, bookmark].sort((a, b) => a.position_ms - b.position_ms),
    });
  }

  /**
   * Removes a bookmark.
   *
   * @param id - The bookmark.
   */
  async removeBookmark(id: string): Promise<void> {
    await this.deps.deleteBookmark(id);
    this.set({ bookmarks: this.state.bookmarks.filter((bookmark) => bookmark.id !== id) });
  }

  /** Marks the loaded edition finished and unloads it. */
  async markFinished(): Promise<void> {
    const target = this.state.target;
    if (!target) return;
    await this.stop();
    await this.deps.setProgress(target.editionId, { is_finished: true });
  }

  /**
   * Ends the session with a last report and unloads the edition.
   *
   * @param keepalive - True while the page is going away.
   */
  async stop(keepalive = false): Promise<void> {
    const session = this.session;
    this.generation += 1;
    this.wantPlay = false;
    this.session = null;
    this.reportAgain = false;
    if (!session) {
      if (this.state.status !== "idle") this.set(idle(this.state.speed));
      return;
    }
    this.tick();
    const body = this.reportBody(session);
    this.audio?.pause();
    if (this.audio) this.audio.src = "";
    this.set(idle(this.state.speed));
    // Cleared before the close is awaited: by the time it answers, another
    // book may be loading, and its state is not this call's to reset.
    this.listened = 0;
    this.tickedAt = null;
    this.pendingSeekMs = null;
    try {
      await this.deps.close(session.session_id, body, keepalive);
    } catch {
      // The last periodic report stands.
    }
  }
}
