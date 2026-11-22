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
  ChapterSpan,
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
  /**
   * Makes the audio element. Called once, on first play.
   *
   * @returns The element the engine plays through.
   */
  audio(): AudioLike;
  /**
   * `GET .../playback`.
   *
   * @param editionId - The edition to read.
   * @returns Its manifest, with the listener's bookmarks.
   */
  manifest(editionId: string): Promise<PlaybackManifest>;
  /**
   * `POST /api/playback/sessions`.
   *
   * @param editionId - The edition to play.
   * @returns The opened session, with its tracks and starting position.
   */
  open(editionId: string): Promise<PlaybackSession>;
  /**
   * `POST .../sync`.
   *
   * @param sessionId - The session reported on.
   * @param report - The position, and the time listened since the last
   *   accepted report.
   * @returns The stored progress. Rejects with `409` once the session ended.
   */
  sync(sessionId: string, report: PlaybackReport): Promise<PlaybackSynced>;
  /**
   * `POST .../close`.
   *
   * @param sessionId - The session to close.
   * @param report - The last position and listened time.
   * @param keepalive - True for a request that must outlive the page.
   * @returns The stored progress.
   */
  close(sessionId: string, report: PlaybackReport, keepalive: boolean): Promise<PlaybackSynced>;
  /**
   * `POST .../bookmarks`.
   *
   * @param editionId - The edition marked.
   * @param positionMs - The place, in milliseconds on its timeline.
   * @param title - The listener's note; may be empty.
   * @returns The stored bookmark.
   */
  addBookmark(editionId: string, positionMs: number, title: string): Promise<Bookmark>;
  /**
   * `DELETE /api/playback/bookmarks/{id}`.
   *
   * @param id - The bookmark to remove.
   * @returns Resolves once it is removed.
   */
  deleteBookmark(id: string): Promise<void>;
  /**
   * `PUT /api/playback/progress/{id}`.
   *
   * @param editionId - The edition whose progress changes.
   * @param edit - `is_finished` marks the edition finished or not.
   * @returns The stored progress.
   */
  setProgress(editionId: string, edit: { is_finished: boolean }): Promise<PlaybackSynced>;
  /**
   * Reads a track's length from its stream.
   *
   * @param path - The track's same-origin stream path.
   * @returns Its length in milliseconds. Rejects when it cannot be read.
   */
  probe(path: string): Promise<number>;
  /**
   * The clock listened time and timers are measured on.
   *
   * @returns Monotonic milliseconds.
   */
  now(): number;
  /**
   * A timer.
   *
   * @param ms - How long to wait, in milliseconds.
   * @returns Resolves after that long.
   */
  wait(ms: number): Promise<void>;
  /**
   * Reads the stored speed.
   *
   * @returns The stored text; `null` when none.
   */
  loadSpeed(): string | null;
  /**
   * Stores the speed for the next visit.
   *
   * @param speed - The playback rate.
   */
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
  /** The loaded edition's chapters, in pre-order; empty when it has none. */
  chapters: ChapterSpan[];
  /** True once the listener reached the end in this sitting. */
  finished: boolean;
  /** What went wrong, in words for the listener. */
  error: string | null;
}

/** Milliseconds between position reports while playing. */
export const SYNC_EVERY_MS = 15_000;

/** Milliseconds before a second stream failure earns another new session. */
const RETRY_AFTER_MS = 60_000;

/**
 * How long a close waits for reports still on their way, in milliseconds,
 * before it goes out assuming the server took them.
 */
export const CLOSE_WAIT_MS = 2000;

/**
 * How long the next book waits for the close of the one before it, in
 * milliseconds, before it opens anyway.
 */
export const SWITCH_WAIT_MS = 5000;

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
    chapters: [],
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

/** One report on its way to the server. */
interface ReportFlight {
  /** The session the report is for. */
  session: PlaybackSession;
  /** True when another report was asked for meanwhile and follows this one. */
  again: boolean;
}

/**
 * The listened time the server has not taken yet, for one stretch of
 * listening to one book: it lives from a load to the stop or failure that
 * ends it, across a replaced session. Reports still on their way hold part
 * of it, so no report sends that part again, and a report that fails gives
 * its part back.
 */
interface Meter {
  /** Milliseconds listened that no answered report carried. */
  listened: number;
  /** The part of `listened` that reports on their way carry. */
  reserved: number;
  /** One promise per report on its way; each resolves when it is answered. */
  pending: Set<Promise<void>>;
}

/**
 * A meter with nothing listened.
 *
 * @returns The meter.
 */
function emptyMeter(): Meter {
  return { listened: 0, reserved: 0, pending: new Set() };
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
  /** Listened time for the book now loaded. */
  private meter: Meter = emptyMeter();
  /**
   * Where the listener was when a session was given up; the next play of
   * that book starts there, whatever the server last stored. `null`
   * otherwise.
   */
  private resumeMs: number | null = null;
  /** {@link PlayerDeps.now} at the last listened-time tick; `null` while paused. */
  private tickedAt: number | null = null;
  /** {@link PlayerDeps.now} at the last report. */
  private syncedAt = 0;
  /**
   * The report in flight for the open session; `null` with none. A report
   * still in flight for a session since replaced is not kept here, so it
   * never holds back the reports of the session that replaced it.
   */
  private flight: ReportFlight | null = null;
  /**
   * True once the open session played its last track to the end. Kept
   * apart from the position, which a seek back changes: the session still
   * finished the book, so playing on takes a new one.
   */
  private ended = false;
  /** The replacement of a failed session under way; `null` with none. */
  private recovery: Promise<void> | null = null;
  /** Closes not yet answered, so a later stop can wait for them too. */
  private readonly closing = new Set<Promise<void>>();
  /**
   * One function per close still waiting for reports; calling it sends that
   * close at once, as a `keepalive` request.
   */
  private readonly hurries = new Set<() => void>();
  /** Counts bookmarks added and removed, so a list read earlier is not shown over them. */
  private bookmarkEdits = 0;
  /** Counts loads, so an answer for an earlier one is dropped. */
  private generation = 0;
  /**
   * Counts the listener's choices ({@link Player.claim}, play and stop), so
   * only the latest one plays.
   */
  private claims = 0;
  /** True between {@link Player.retire} and {@link Player.readmit}: nothing plays. */
  private retired = false;
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
   * Marks the start of a request to play that has something to read
   * first. The caller asks the returned check once it has read, and drops
   * its request when the listener has since chosen, started or stopped
   * a book. A session that fails meanwhile is not a choice and leaves the
   * request standing.
   *
   * @returns True while this is the listener's latest choice.
   */
  claim(): () => boolean {
    const claim = (this.claims += 1);
    return () => claim === this.claims;
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
   * played before. Asking for the loaded edition resumes it. Nothing is
   * played after {@link Player.retire} until {@link Player.readmit}.
   *
   * @param target - What to play.
   */
  async play(target: PlayTarget): Promise<void> {
    if (this.retired) return;
    // A choice in its own right, the loaded book included: a request still
    // reading for another book no longer stands.
    this.claims += 1;
    if (this.state.target?.editionId === target.editionId && this.session) {
      this.resume();
      return;
    }
    // A book given up after a failure goes on from where the listener was.
    const again = this.state.target?.editionId === target.editionId;
    await this.begin(target, again ? this.resumeMs : null);
  }

  /**
   * Opens a new session for an edition, closing whatever played before,
   * the same edition's earlier session included.
   *
   * @param target - What to play.
   * @param startMs - Where to start; `null` for where the server says the
   *   listener left it.
   */
  private async begin(target: PlayTarget, startMs: number | null = null): Promise<void> {
    // Everything that decides which request is the latest happens before
    // the first await, so a slow close of the book before cannot let an
    // earlier request overtake a later one.
    const closing = this.unload();
    const generation = (this.generation += 1);
    this.retriedAt = null;
    this.wantPlay = true;
    this.set({ ...idle(this.state.speed), status: "loading", target });
    // The close goes first so the server sees the books in order, but one
    // that does not answer must not keep the next book from starting.
    await Promise.race([closing, this.deps.wait(SWITCH_WAIT_MS)]);
    if (generation !== this.generation) return;
    try {
      const session = await this.deps.open(target.editionId);
      if (generation !== this.generation) {
        void this.deps
          .close(session.session_id, { position_ms: session.position_ms }, false)
          .catch(() => undefined);
        return;
      }
      await this.adopt(session, startMs ?? session.position_ms, generation);
      if (generation !== this.generation) return;
      // Bookmarks are a convenience: playing does not wait on them.
      void this.readBookmarks(target.editionId, generation);
    } catch (err) {
      if (generation === this.generation) {
        this.session = null;
        // The place is still the one to go on from when play is pressed again.
        this.resumeMs = startMs;
        this.set({ status: "error", error: playbackErrorText(err) });
      }
    }
  }

  /**
   * Reads the edition's bookmarks and chapters. A list read while the
   * listener added or removed a bookmark may be older than that change,
   * so it is read again.
   *
   * @param editionId - The loaded edition.
   * @param generation - The load the list belongs to.
   */
  private async readBookmarks(editionId: string, generation: number): Promise<void> {
    try {
      // Read until a list arrives that no change overtook. A listener
      // cannot add bookmarks faster than they are read for long.
      for (;;) {
        const edits = this.bookmarkEdits;
        const manifest = await this.deps.manifest(editionId);
        if (generation !== this.generation) return;
        if (edits === this.bookmarkEdits) {
          this.set({ bookmarks: manifest.bookmarks, chapters: manifest.chapters });
          return;
        }
      }
    } catch {
      // The bookmarks added in this visit still show.
    }
  }

  /**
   * Takes a session's tracks and loads the one `positionMs` falls in.
   *
   * A session that cannot be taken (its tracks cannot be read, or the
   * listener moved on) is closed here, since nothing else knows of it.
   *
   * @param session - The session just opened.
   * @param startMs - Where to start on the timeline; `null` for where the
   *   listener is once the tracks are known, which keeps a seek made while
   *   a replacement session was being prepared.
   * @param generation - The load this answer belongs to.
   */
  private async adopt(
    session: PlaybackSession,
    startMs: number | null,
    generation: number,
  ): Promise<void> {
    const discard = () =>
      void this.deps
        .close(session.session_id, { position_ms: session.position_ms }, false)
        .catch(() => undefined);
    const paths = session.tracks.map((track) => track.path ?? "");
    let durations: number[];
    try {
      if (paths.length === 0 || paths.some((path) => !path)) {
        throw new Error("the session lists no tracks");
      }
      // One file is its own timeline. Several need every length before a
      // position can be placed in one of them.
      durations =
        paths.length === 1
          ? [Number.POSITIVE_INFINITY]
          : await Promise.all(paths.map((path) => this.deps.probe(path)));
    } catch (err) {
      discard();
      throw err;
    }
    if (generation !== this.generation) {
      discard();
      return;
    }
    const positionMs = startMs ?? this.state.positionMs;
    this.session = session;
    this.ended = false;
    this.durations = durations;
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
    if (this.state.status === "playing") {
      // A new source pauses the element without telling anyone. Until the
      // track is ready the book is loading, so pause and play act on that.
      this.tick();
      this.tickedAt = null;
      this.set({ status: "loading" });
    }
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
      this.meter.listened += Math.min(Math.max(0, now - this.tickedAt), 2000);
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
      // Paused by the listener or by the browser (another app took the
      // audio): either way nothing should start it again unasked. A track
      // that ends says so itself, after this.
      this.wantPlay = false;
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
    this.ended = true;
    this.set({ status: "paused", positionMs: this.state.durationMs ?? this.position() });
    await this.report();
  }

  /**
   * The stream failed. A session that ended (a day idle, changed files) is
   * the usual cause, so open one new session at the same place.
   */
  private async onStreamError(): Promise<void> {
    if (!this.session || !this.state.target) return;
    // The stream being replaced has nothing new to say by failing again.
    if (this.recovery) return;
    const now = this.deps.now();
    // A stream that fails again right after a new session is broken, not ended.
    if (this.retriedAt !== null && now - this.retriedAt < RETRY_AFTER_MS) {
      this.abandon(playbackErrorText(null));
      return;
    }
    this.retriedAt = now;
    await this.reopen();
  }

  /**
   * Gives up on the open session and shows why. The book stays chosen, at
   * the place the listener reached, so pressing play opens a new session.
   * With no session nothing reports or stops what is buffered, so the
   * audio goes with it.
   *
   * @param error - Words for the listener.
   */
  private abandon(error: string): void {
    const session = this.session;
    this.tick();
    const body = this.reportBody();
    const meter = this.meter;
    this.meter = emptyMeter();
    // Whatever was on its way for this session, a replacement included, is
    // no longer wanted.
    this.generation += 1;
    this.recovery = null;
    this.session = null;
    this.ended = false;
    this.wantPlay = false;
    this.tickedAt = null;
    this.pendingSeekMs = null;
    this.resumeMs = body.position_ms;
    this.audio?.pause();
    if (this.audio) this.audio.src = "";
    this.set({ status: "error", error });
    // The server may have ended it already; then the close changes nothing.
    if (session) void this.end(session, meter, body, false);
  }

  /**
   * Closes a session and keeps the close where {@link Player.stop} finds
   * it until it is answered.
   *
   * @param session - The session to close.
   * @param meter - The listened time of the stretch that ended.
   * @param body - The close report.
   * @param keepalive - True while the page is going away.
   * @returns Settles when the close was answered or failed.
   */
  private end(
    session: PlaybackSession,
    meter: Meter,
    body: PlaybackReport,
    keepalive: boolean,
  ): Promise<void> {
    const closing: Promise<void> = this.finish(session, meter, body, keepalive).finally(() => {
      this.closing.delete(closing);
    });
    this.closing.add(closing);
    return closing;
  }

  /**
   * Sends a session's close. Reports still on their way are given a moment
   * to be answered first, so the close carries the time of one that failed
   * and leaves out the time of one the server took. One that has not
   * answered by then is taken as accepted.
   *
   * @param session - The session to close.
   * @param meter - The listened time of the stretch that ended.
   * @param body - The close report; its listened time is filled in here.
   * @param keepalive - True while the page is going away, which cannot wait.
   */
  private async finish(
    session: PlaybackSession,
    meter: Meter,
    body: PlaybackReport,
    keepalive: boolean,
  ): Promise<void> {
    let urgent = keepalive;
    if (!keepalive && meter.pending.size > 0) {
      // The page may go away during the wait; then this close cannot wait
      // either, and must outlive the page.
      let hurry = () => {};
      const hurried = new Promise<void>((resolve) => {
        hurry = () => {
          urgent = true;
          resolve();
        };
      });
      this.hurries.add(hurry);
      await Promise.race([Promise.all([...meter.pending]), this.deps.wait(CLOSE_WAIT_MS), hurried]);
      this.hurries.delete(hurry);
    }
    body.listened_ms = Math.round(Math.max(0, meter.listened - meter.reserved));
    try {
      await this.deps.close(session.session_id, body, urgent);
    } catch {
      // The last periodic report stands.
    }
  }

  /**
   * Replaces the session with a new one at the place the listener is. A
   * failed report and a failed stream can both ask while one replacement
   * is under way; they share it.
   *
   * @returns Settles when the replacement was taken or given up.
   */
  private reopen(): Promise<void> {
    if (this.recovery) return this.recovery;
    const recovery: Promise<void> = this.replace().finally(() => {
      if (this.recovery === recovery) this.recovery = null;
    });
    this.recovery = recovery;
    return recovery;
  }

  /** Opens the replacement session {@link Player.reopen} asked for. */
  private async replace(): Promise<void> {
    const target = this.state.target;
    if (!target) return;
    const generation = this.generation;
    // Decided before the await, so a pause pressed meanwhile still holds.
    this.wantPlay = this.wantPlay || this.state.status === "playing";
    try {
      const session = await this.deps.open(target.editionId);
      if (generation !== this.generation) {
        void this.deps
          .close(session.session_id, { position_ms: session.position_ms }, false)
          .catch(() => undefined);
        return;
      }
      await this.adopt(session, null, generation);
    } catch (err) {
      if (generation === this.generation) this.abandon(playbackErrorText(err));
    }
  }

  /**
   * The report for the current position, with the listened time no report
   * on its way already carries.
   *
   * @returns The body of a sync or close request.
   */
  private reportBody(): PlaybackReport {
    const body: PlaybackReport = {
      position_ms: Math.max(0, Math.round(this.state.positionMs)),
      listened_ms: Math.round(Math.max(0, this.meter.listened - this.meter.reserved)),
    };
    if (this.state.durationMs) body.duration_ms = Math.round(this.state.durationMs);
    return body;
  }

  /**
   * Sends a report; a session the server ended is replaced. A report asked
   * for while one is in flight for the same session (a pause, the end of
   * the book) follows it with the position as it then is.
   *
   * @param replaced - True for the one report that follows a replaced
   *   session, which is not itself followed by another replacement's.
   */
  private async report(replaced = false): Promise<void> {
    const session = this.session;
    if (!session) return;
    if (this.flight?.session === session) {
      this.flight.again = true;
      return;
    }
    this.syncedAt = this.deps.now();
    const meter = this.meter;
    const body = this.reportBody();
    const carried = Math.max(0, meter.listened - meter.reserved);
    meter.reserved += carried;
    let answered = () => {};
    const pending = new Promise<void>((resolve) => {
      answered = resolve;
    });
    meter.pending.add(pending);
    const flight: ReportFlight = { session, again: false };
    this.flight = flight;
    let synced: PlaybackSynced | null = null;
    let failure: unknown = null;
    try {
      synced = await this.deps.sync(session.session_id, body);
    } catch (err) {
      failure = err;
    }
    // Settled before anything else is sent: time the server took leaves the
    // meter whichever session is open by now, and time it refused is free
    // for the next report.
    if (synced) meter.listened = Math.max(0, meter.listened - carried);
    meter.reserved -= carried;
    meter.pending.delete(pending);
    answered();
    if (this.flight === flight) this.flight = null;
    let resend = false;
    if (synced) {
      // An answer for a session since replaced says nothing about this one.
      if (this.session === session && synced.finished_now) this.set({ finished: true });
    } else if (isApiError(failure) && failure.status === 409 && !replaced) {
      // A new session that refuses its first report (`replaced`) is left to
      // the next periodic one, so two failures cannot open sessions in a
      // loop. A failed stream may have replaced the session already; then
      // there is nothing to open.
      if (this.session === session) await this.reopen();
      // The server never took this report, and a paused book sends no
      // other: the session now open for the same stretch of listening gets
      // the place and the time once.
      resend = this.session !== null && this.session !== session && this.meter === meter;
    }
    // Any other failure is retried by the next report.
    if (resend) await this.report(true);
    else if (flight.again && this.session === session) await this.report();
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
    const atEnd = Boolean(duration) && this.state.positionMs >= (duration ?? 0);
    if ((atEnd || this.ended) && this.state.target) {
      // Played to the end: listening again is a new session. Opening one is
      // what tells the server the book is in progress again; reports on the
      // session that finished it leave it finished. From the end the book
      // starts over; after a seek back it goes on from there.
      void this.begin(this.state.target, atEnd ? null : this.state.positionMs);
      return;
    }
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
    this.bookmarkEdits += 1;
    if (this.state.target?.editionId !== target.editionId) return;
    const others = this.state.bookmarks.filter((known) => known.id !== bookmark.id);
    this.set({
      bookmarks: [...others, bookmark].sort((a, b) => a.position_ms - b.position_ms),
    });
  }

  /**
   * Removes a bookmark.
   *
   * @param id - The bookmark.
   */
  async removeBookmark(id: string): Promise<void> {
    await this.deps.deleteBookmark(id);
    this.bookmarkEdits += 1;
    this.set({ bookmarks: this.state.bookmarks.filter((bookmark) => bookmark.id !== id) });
  }

  /**
   * Marks the loaded edition finished and unloads it. When the mark cannot
   * be stored, the book comes back as the one chosen, with the failure in
   * {@link PlayerState.error}, so whatever shows the player can say so and
   * offer the action again.
   *
   * @throws What the route threw, after the state shows it.
   */
  async markFinished(): Promise<void> {
    const target = this.state.target;
    if (!target) return;
    await this.stop();
    try {
      await this.deps.setProgress(target.editionId, { is_finished: true });
    } catch (err) {
      // Unless the listener started something else meanwhile.
      if (this.state.target === null) {
        this.set({
          status: "error",
          target,
          error: "The book could not be marked finished. Try again.",
        });
      }
      throw err;
    }
  }

  /**
   * Ends the session with a last report and unloads the edition. It also
   * waits for closes sent earlier and not yet answered, so a caller that
   * must not go on before the last report (a sign-out) can rely on it even
   * when the listener had already pressed stop. While the page is going
   * away, a close still waiting for reports is sent at once instead.
   *
   * @param keepalive - True while the page is going away.
   */
  async stop(keepalive = false): Promise<void> {
    if (keepalive) for (const hurry of this.hurries) hurry();
    await this.unload(keepalive);
    await Promise.all([...this.closing]);
  }

  /**
   * Stops as {@link Player.stop} does and refuses every later
   * {@link Player.play} until {@link Player.readmit}. For a change of the
   * signed-in identity: a book started while the last close is still out
   * would be opened as the identity being left and never closed as it.
   *
   * @returns Resolves when every close was answered.
   */
  async retire(): Promise<void> {
    this.retired = true;
    await this.stop();
  }

  /**
   * Lets books play again after {@link Player.retire}. A request made
   * meanwhile and still reading ({@link Player.claim}) is dropped: it was
   * made as the identity that has since been left.
   */
  readmit(): void {
    this.retired = false;
    this.claims += 1;
  }

  /**
   * Unloads the edition and closes its session, waiting for that close
   * only: the next book does not wait on closes of books before the last.
   *
   * @param keepalive - True while the page is going away.
   */
  private async unload(keepalive = false): Promise<void> {
    const session = this.session;
    this.generation += 1;
    this.claims += 1;
    this.wantPlay = false;
    this.session = null;
    this.ended = false;
    this.recovery = null;
    this.resumeMs = null;
    if (!session) {
      if (this.state.status !== "idle") this.set(idle(this.state.speed));
      return;
    }
    this.tick();
    const body = this.reportBody();
    this.audio?.pause();
    if (this.audio) this.audio.src = "";
    this.set(idle(this.state.speed));
    // Replaced before the close is awaited: by the time it answers, another
    // book may be loading, and its state is not this call's to reset.
    const meter = this.meter;
    this.meter = emptyMeter();
    this.tickedAt = null;
    this.pendingSeekMs = null;
    await this.end(session, meter, body, keepalive);
  }
}
