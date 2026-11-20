import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import type { Bookmark, PlaybackProgress, PlaybackReport, PlaybackSession } from "./playback";
import {
  Player,
  SKIP_MS,
  SWITCH_WAIT_MS,
  SYNC_EVERY_MS,
  playbackErrorText,
  type AudioLike,
  type PlayTarget,
  type PlayerDeps,
} from "./player";

/**
 * An audio element the tests move by hand. Nothing happens on its own:
 * assigning `src` loads nothing, and metadata, time and errors arrive only
 * when a test calls {@link FakeAudio.ready}, {@link FakeAudio.at} or
 * {@link FakeAudio.emit}. `play()` and `pause()` send their events before
 * they return, where a browser sends them a moment later, so a test of an
 * order between those events and something else arranges it by hand.
 */
class FakeAudio implements AudioLike {
  /** The address last assigned; assigning it does not pause or load. */
  src = "";
  /** Seconds into the loaded file. */
  currentTime = 0;
  /** The loaded file's seconds; `NaN` until {@link FakeAudio.ready}. */
  duration = Number.NaN;
  /** True unless `play()` started it. */
  paused = true;
  /** The speed last assigned. */
  playbackRate = 1;
  /** What `play()` rejects with; `null` to start. */
  refuse: unknown = null;
  /** The listeners added, by event name. */
  private readonly listeners = new Map<string, (() => void)[]>();

  /**
   * Starts, sending `play` before returning, or rejects with
   * {@link FakeAudio.refuse}.
   *
   * @returns Settles at once.
   */
  play(): Promise<void> {
    if (this.refuse) return Promise.reject(this.refuse);
    this.paused = false;
    this.emit("play");
    return Promise.resolve();
  }

  /** Pauses, sending `pause` before returning; nothing when already paused. */
  pause(): void {
    if (this.paused) return;
    this.paused = true;
    this.emit("pause");
  }

  /**
   * Adds a listener.
   *
   * @param type - The event name.
   * @param listener - Called on each {@link FakeAudio.emit} of it.
   */
  addEventListener(type: string, listener: () => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  /**
   * Sends an event to its listeners, in the order they were added.
   *
   * @param type - The event name, such as `ended` or `error`.
   */
  emit(type: string): void {
    for (const listener of this.listeners.get(type) ?? []) listener();
  }

  /**
   * The loaded file's metadata arrives.
   *
   * @param seconds - The file's length.
   */
  ready(seconds: number): void {
    this.duration = seconds;
    this.emit("loadedmetadata");
  }

  /**
   * Playback reaches a place in the loaded file.
   *
   * @param seconds - The place.
   */
  at(seconds: number): void {
    this.currentTime = seconds;
    this.emit("timeupdate");
  }
}

/** The book every test plays unless it names another edition. */
const TARGET: PlayTarget = {
  editionId: "ed1",
  workId: "w1",
  title: "Piranesi",
  authors: "Susanna Clarke",
  coverPath: "/api/covers/abc",
};

/**
 * A progress row as the server answers a report with.
 *
 * @param positionMs - The stored position.
 * @param finished - True for a finished edition.
 * @returns The row.
 */
function progress(positionMs: number, finished = false): PlaybackProgress {
  return {
    edition_id: "ed1",
    work_id: "w1",
    position_ms: positionMs,
    duration_ms: null,
    is_finished: finished,
    started_at: "2026-11-19T10:00:00Z",
    finished_at: null,
    updated_at: "2026-11-19T10:00:00Z",
  };
}

/**
 * A session as the server opens one.
 *
 * @param id - The session id, which its track addresses carry.
 * @param positionMs - Where the server says to start.
 * @param tracks - How many files the edition has.
 * @returns The session.
 */
function session(id: string, positionMs: number, tracks = 1): PlaybackSession {
  return {
    session_id: id,
    edition_id: "ed1",
    work_id: "w1",
    position_ms: positionMs,
    duration_ms: null,
    idle_secs: 86_400,
    tracks: Array.from({ length: tracks }, (_, index) => ({
      index,
      path: `/api/playback/sessions/${id}/tracks/${index}`,
      content_type: "audio/mp4",
      bitrate_kbps: null,
      lossless: false,
    })),
  };
}

/**
 * A player over fakes, with what it sent.
 *
 * A hold is read when a request is made, not when the test sets it, and
 * delays the answer of every request made while it is set, after the
 * request is recorded. A failure is read and thrown when a request is
 * made, before the request is recorded or held, with two exceptions that
 * are read once the hold lets go: `failures.close` (the close is recorded
 * and held first) and `failures.syncLate` (the report is recorded and held
 * first). `failures.syncOnce` fails one report and clears itself.
 * `holds.wait` stands in for the timer a close waits on; without it the
 * timer has always run out. `holds.switchWait` stands in for the timer the
 * next book waits on for the close of the one before; without it that
 * timer never runs out.
 *
 * @param options - `tracks` and `lengths` (milliseconds per track) shape
 *   the edition; `startMs` is where the server says to start.
 * @returns The player, its fakes and the requests it made.
 */
function world(options: { tracks?: number; startMs?: number; lengths?: number[] } = {}) {
  const audio = new FakeAudio();
  const clock = { now: 0 };
  const opened: string[] = [];
  const synced: { id: string; report: PlaybackReport }[] = [];
  const closed: { id: string; report: PlaybackReport; keepalive: boolean }[] = [];
  const finished: string[] = [];
  const saved: number[] = [];
  const failures: {
    open?: unknown;
    sync?: unknown;
    syncOnce?: unknown;
    syncLate?: unknown;
    close?: unknown;
    manifest?: unknown;
    probe?: unknown;
    setProgress?: unknown;
  } = {};
  // Requests wait on these before they answer.
  const holds: {
    open?: Promise<void>;
    sync?: Promise<void>;
    close?: Promise<void>;
    manifest?: Promise<void>;
    wait?: Promise<void>;
    switchWait?: Promise<void>;
  } = {};
  const bookmarks: Bookmark[] = [
    {
      id: "b1",
      edition_id: "ed1",
      position_ms: 5000,
      title: "",
      created_at: "2026-11-19T10:00:00Z",
    },
  ];
  const flags = { finishedNow: false };
  const deps: PlayerDeps = {
    audio: () => audio,
    manifest: async (editionId) => {
      if (failures.manifest) throw failures.manifest;
      // What the server held when it was asked, however late the answer.
      const snapshot = [...bookmarks];
      await holds.manifest;
      return {
        edition_id: editionId,
        work_id: "w1",
        title: "Piranesi",
        playable: true,
        duration_ms: null,
        tracks: [],
        progress: null,
        bookmarks: snapshot,
      };
    },
    open: async (editionId) => {
      if (failures.open) throw failures.open;
      opened.push(editionId);
      const id = `s${opened.length}`;
      await holds.open;
      return session(id, options.startMs ?? 0, options.tracks ?? 1);
    },
    sync: async (id, report) => {
      if (failures.syncOnce) {
        const failure = failures.syncOnce;
        failures.syncOnce = undefined;
        throw failure;
      }
      if (failures.sync) throw failures.sync;
      synced.push({ id, report });
      await holds.sync;
      if (failures.syncLate) throw failures.syncLate;
      return { progress: progress(report.position_ms), finished_now: flags.finishedNow };
    },
    close: async (id, report, keepalive) => {
      closed.push({ id, report, keepalive });
      await holds.close;
      if (failures.close) throw failures.close;
      return { progress: progress(report.position_ms), finished_now: false };
    },
    addBookmark: async (editionId, positionMs, title) => {
      const bookmark: Bookmark = {
        id: `b${positionMs}`,
        edition_id: editionId,
        position_ms: positionMs,
        title,
        created_at: "2026-11-19T10:00:00Z",
      };
      bookmarks.push(bookmark);
      return bookmark;
    },
    deleteBookmark: async () => undefined,
    setProgress: async (editionId) => {
      if (failures.setProgress) throw failures.setProgress;
      finished.push(editionId);
      return { progress: progress(0, true), finished_now: true };
    },
    probe: async (path) => {
      if (failures.probe) throw failures.probe;
      return (options.lengths ?? [])[Number(path.split("/").pop())] ?? 0;
    },
    now: () => clock.now,
    wait: (ms) =>
      ms === SWITCH_WAIT_MS
        ? (holds.switchWait ?? new Promise<void>(() => {}))
        : (holds.wait ?? Promise.resolve()),
    loadSpeed: () => "1.5",
    saveSpeed: (speed) => saved.push(speed),
  };
  const player = new Player(deps);
  return { player, audio, clock, opened, synced, closed, finished, saved, failures, flags, holds };
}

/**
 * Lets resolved promises run.
 *
 * @returns Settles on the next timer turn, after every pending microtask.
 */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("Player", () => {
  it("starts idle at the stored speed", () => {
    const { player } = world();
    expect(player.state.status).toBe("idle");
    expect(player.state.speed).toBe(1.5);
  });

  it("opens a session and plays from where the listener left off", async () => {
    const w = world({ startMs: 90_000 });
    const seen: string[] = [];
    const stop = w.player.subscribe(() => seen.push(w.player.state.status));
    await w.player.play(TARGET);
    expect(w.player.state.status).toBe("loading");
    expect(w.audio.src).toBe("/api/playback/sessions/s1/tracks/0");
    w.audio.ready(3600);
    expect(w.audio.currentTime).toBe(90);
    expect(w.audio.playbackRate).toBe(1.5);
    expect(w.player.state.status).toBe("playing");
    expect(w.player.state.durationMs).toBe(3_600_000);
    await settle();
    expect(w.player.state.bookmarks.map((bookmark) => bookmark.id)).toEqual(["b1"]);
    expect(seen).toContain("loading");
    stop();
    const before = seen.length;
    w.audio.at(91);
    expect(seen.length).toBe(before);
  });

  it("reports the position every fifteen seconds and when paused", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.clock.now = 1000;
    w.audio.at(1);
    w.clock.now = 2000;
    w.audio.at(2);
    expect(w.synced).toHaveLength(0);
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced).toHaveLength(1);
    expect(w.synced[0]).toEqual({
      id: "s1",
      report: { position_ms: 15_000, listened_ms: 4000, duration_ms: 600_000 },
    });
    w.clock.now += 1000;
    w.audio.at(16);
    w.player.pause();
    await settle();
    expect(w.player.state.status).toBe("paused");
    expect(w.synced).toHaveLength(2);
    expect(w.synced[1].report).toMatchObject({ position_ms: 16_000, listened_ms: 1000 });
  });

  it("does not count a suspended tab as listening", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.clock.now = 3_600_000;
    w.audio.at(5);
    await settle();
    expect(w.synced[0].report.listened_ms).toBe(2000);
  });

  it("resumes the loaded edition instead of opening it again", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.player.toggle();
    expect(w.player.state.status).toBe("paused");
    await w.player.play(TARGET);
    expect(w.opened).toHaveLength(1);
    expect(w.player.state.status).toBe("playing");
    w.player.toggle();
    expect(w.audio.paused).toBe(true);
  });

  it("seeks and skips inside the runtime", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(100);
    w.player.seek(40_000);
    expect(w.audio.currentTime).toBe(40);
    w.player.skip(SKIP_MS);
    expect(w.audio.currentTime).toBe(70);
    w.player.skip(SKIP_MS * 4);
    expect(w.player.state.positionMs).toBe(100_000);
    w.player.skip(-SKIP_MS * 9);
    expect(w.audio.currentTime).toBe(0);
  });

  it("keeps a seek made before the file is ready", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.player.seek(12_000);
    expect(w.player.state.positionMs).toBe(12_000);
    w.audio.ready(100);
    expect(w.audio.currentTime).toBe(12);
  });

  it("plays several files as one timeline", async () => {
    const w = world({ tracks: 3, startMs: 70_000, lengths: [60_000, 60_000, 30_000] });
    await w.player.play(TARGET);
    expect(w.player.state.durationMs).toBe(150_000);
    expect(w.audio.src).toBe("/api/playback/sessions/s1/tracks/1");
    w.audio.ready(60);
    expect(w.audio.currentTime).toBe(10);
    w.audio.at(20);
    expect(w.player.state.positionMs).toBe(80_000);

    w.audio.emit("ended");
    expect(w.audio.src).toBe("/api/playback/sessions/s1/tracks/2");
    w.audio.ready(30);
    expect(w.audio.currentTime).toBe(0);
    expect(w.player.state.status).toBe("playing");

    w.player.seek(5000);
    expect(w.audio.src).toBe("/api/playback/sessions/s1/tracks/0");
    w.audio.ready(60);
    expect(w.audio.currentTime).toBe(5);
  });

  it("stops at the end and reports it", async () => {
    const w = world();
    w.flags.finishedNow = true;
    await w.player.play(TARGET);
    w.audio.ready(100);
    w.audio.at(100);
    w.audio.paused = true;
    w.audio.emit("ended");
    await settle();
    expect(w.player.state.status).toBe("paused");
    expect(w.player.state.positionMs).toBe(100_000);
    expect(w.player.state.finished).toBe(true);
    expect(w.synced.at(-1)?.report.position_ms).toBe(100_000);
  });

  it("opens a new session to listen again to a book played to its end", async () => {
    const w = world();
    w.flags.finishedNow = true;
    await w.player.play(TARGET);
    w.audio.ready(100);
    w.audio.at(100);
    w.audio.paused = true;
    w.audio.emit("ended");
    await settle();
    w.player.resume();
    await settle();
    // The session that finished the book is closed before the next opens.
    expect(w.closed.map((c) => c.id)).toEqual(["s1"]);
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    expect(w.player.state.finished).toBe(false);
    expect(w.player.state.positionMs).toBe(0);
  });

  it("pauses when the sleep timer runs out", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(6000);
    expect(w.player.sleepRemainingMs()).toBeNull();
    w.player.setSleep(15);
    w.clock.now = 60_000;
    expect(w.player.sleepRemainingMs()).toBe(14 * 60_000);
    w.audio.at(60);
    expect(w.player.state.status).toBe("playing");
    w.clock.now = 15 * 60_000;
    w.audio.at(900);
    expect(w.player.state.status).toBe("paused");
    expect(w.player.state.sleepAt).toBeNull();
    w.player.setSleep(5);
    w.player.setSleep(null);
    expect(w.player.state.sleepAt).toBeNull();
  });

  it("changes speed, remembers it and applies it to the element", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(100);
    w.player.setSpeed(2.1);
    expect(w.player.state.speed).toBe(2);
    expect(w.audio.playbackRate).toBe(2);
    expect(w.saved).toEqual([2]);
  });

  it("opens a new session when the server ended this one", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.failures.sync = new ApiError(409, "session_ended", { code: "session_ended" });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.opened).toHaveLength(2);
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    w.audio.ready(600);
    expect(w.audio.currentTime).toBe(15);
    expect(w.player.state.status).toBe("playing");
  });

  it("keeps playing through a report that fails for another reason", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.failures.sync = new ApiError(503, "unavailable", { code: "unavailable" });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.opened).toHaveLength(1);
    expect(w.player.state.status).toBe("playing");
    w.failures.sync = undefined;
    w.clock.now = SYNC_EVERY_MS * 2;
    w.audio.at(30);
    await settle();
    expect(w.synced.at(-1)?.report.listened_ms).toBe(4000);
  });

  it("retries a failed stream once, then says so", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.audio.at(30);
    w.audio.emit("error");
    await settle();
    expect(w.opened).toHaveLength(2);
    w.audio.emit("error");
    await settle();
    expect(w.opened).toHaveLength(2);
    expect(w.player.state.status).toBe("error");
    expect(w.player.state.error).toBe("This book could not be played.");
  });

  it("gives a stream that failed long after the last retry another session", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.audio.emit("error");
    await settle();
    w.audio.ready(600);
    w.clock.now = 120_000;
    w.audio.emit("error");
    await settle();
    expect(w.opened).toHaveLength(3);
    expect(w.player.state.status).not.toBe("error");
  });

  it("says why a book cannot start", async () => {
    const w = world();
    w.failures.open = new ApiError(404, "not_playable", { code: "not_playable" });
    await w.player.play(TARGET);
    expect(w.player.state.status).toBe("error");
    expect(w.player.state.error).toBe("This book cannot be played from this server.");
    expect(w.player.state.target).toEqual(TARGET);
    w.failures.open = undefined;
    w.player.resume();
    await settle();
    expect(w.opened).toHaveLength(1);
  });

  it("stays paused with a hint when the browser blocks playback", async () => {
    const w = world();
    w.audio.refuse = new DOMException("blocked", "NotAllowedError");
    await w.player.play(TARGET);
    w.audio.ready(600);
    await settle();
    expect(w.player.state.status).toBe("paused");
    expect(w.player.state.error).toBe("The browser blocked playback. Press play again.");
    w.audio.refuse = new DOMException("interrupted", "AbortError");
    w.player.resume();
    await settle();
    expect(w.player.state.error).toBeNull();
  });

  it("adds and removes bookmarks in timeline order", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    await settle();
    w.audio.at(2);
    await w.player.addBookmark("the hall");
    expect(w.player.state.bookmarks.map((bookmark) => bookmark.position_ms)).toEqual([2000, 5000]);
    await w.player.removeBookmark("b1");
    expect(w.player.state.bookmarks.map((bookmark) => bookmark.id)).toEqual(["b2000"]);
  });

  it("closes the session with a last report when stopped", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.clock.now = 1000;
    w.audio.at(42);
    w.failures.close = new Error("offline");
    await w.player.stop(true);
    expect(w.closed).toEqual([
      {
        id: "s1",
        report: { position_ms: 42_000, listened_ms: 1000, duration_ms: 600_000 },
        keepalive: true,
      },
    ]);
    expect(w.player.state.status).toBe("idle");
    expect(w.audio.src).toBe("");
    await w.player.stop();
    expect(w.closed).toHaveLength(1);
  });

  it("closes what played before when another book starts", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    await w.player.play({ ...TARGET, editionId: "ed2" });
    expect(w.closed.map((close) => close.id)).toEqual(["s1"]);
    expect(w.opened).toEqual(["ed1", "ed2"]);
    expect(w.player.state.target?.editionId).toBe("ed2");
  });

  it("drops a session that answers after the listener moved on", async () => {
    const w = world();
    let release = () => {};
    w.holds.open = new Promise<void>((resolve) => {
      release = resolve;
    });
    const first = w.player.play(TARGET);
    await settle();
    expect(w.opened).toEqual(["ed1"]);
    const second = w.player.play({ ...TARGET, editionId: "ed2" });
    release();
    await Promise.all([first, second]);
    await settle();
    expect(w.player.state.target?.editionId).toBe("ed2");
    expect(w.closed.map((close) => close.id)).toEqual(["s1"]);
  });

  it("opens nothing for a request another replaced at once", async () => {
    const w = world();
    const first = w.player.play(TARGET);
    const second = w.player.play({ ...TARGET, editionId: "ed2" });
    await Promise.all([first, second]);
    expect(w.opened).toEqual(["ed2"]);
    expect(w.closed).toEqual([]);
  });

  it("starts nothing between retiring and being readmitted", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.close = new Promise<void>((resolve) => {
      release = resolve;
    });
    const retired = w.player.retire();
    // Chosen while the close for the identity being left is still out.
    await w.player.play({ ...TARGET, editionId: "ed2" });
    await settle();
    expect(w.player.state.status).toBe("idle");
    release();
    await retired;
    await w.player.play({ ...TARGET, editionId: "ed2" });
    expect(w.opened).toEqual(["ed1"]);
    w.player.readmit();
    await w.player.play({ ...TARGET, editionId: "ed2" });
    expect(w.opened).toEqual(["ed1", "ed2"]);
  });

  it("lets the latest of several requests win over a slow close", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.close = new Promise<void>((resolve) => {
      release = resolve;
    });
    const second = w.player.play({ ...TARGET, editionId: "ed2" });
    const third = w.player.play({ ...TARGET, editionId: "ed3" });
    await settle();
    expect(w.player.state.target?.editionId).toBe("ed3");
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    w.player.seek(30_000);
    release();
    await Promise.all([second, third]);
    await settle();
    expect(w.opened).toEqual(["ed1", "ed3"]);
    expect(w.player.state.target?.editionId).toBe("ed3");
    // The seek made while the first book's close was pending is kept.
    w.audio.ready(600);
    expect(w.audio.currentTime).toBe(30);
    expect(w.player.state.status).toBe("playing");
  });

  it("stays loaded and paused when paused before the file is ready", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.player.toggle();
    expect(w.player.state.status).toBe("paused");
    w.audio.ready(600);
    expect(w.audio.paused).toBe(true);
    expect(w.player.state.status).toBe("paused");
    w.player.toggle();
    expect(w.audio.paused).toBe(false);
    expect(w.player.state.status).toBe("playing");
  });

  it("plays when resumed before the file is ready", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.player.toggle();
    w.player.toggle();
    expect(w.player.state.status).toBe("loading");
    w.audio.ready(600);
    expect(w.player.state.status).toBe("playing");
  });

  it("does not start a book paused while its session opened", async () => {
    const w = world();
    const playing = w.player.play(TARGET);
    w.player.toggle();
    await playing;
    expect(w.player.state.status).toBe("paused");
    w.audio.ready(600);
    expect(w.audio.paused).toBe(true);
    w.player.toggle();
    expect(w.player.state.status).toBe("playing");
  });

  it("follows a report in flight with the place the listener paused at", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced).toHaveLength(1);
    w.clock.now += 1000;
    w.audio.at(16);
    w.player.pause();
    await settle();
    expect(w.synced).toHaveLength(1);
    release();
    await settle();
    expect(w.synced).toHaveLength(2);
    expect(w.synced[1].report.position_ms).toBe(16_000);
  });

  it("does not send listened time twice when stopped during a report", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.sync = new Promise<void>(() => {});
    w.clock.now = 1000;
    w.audio.at(1);
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced[0].report.listened_ms).toBe(3000);
    w.clock.now += 1000;
    w.audio.at(16);
    await w.player.stop();
    expect(w.closed[0].report).toMatchObject({ position_ms: 16_000, listened_ms: 1000 });
  });

  it("stops the audio when a session that ended cannot be replaced", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.failures.sync = new ApiError(409, "session_ended", { code: "session_ended" });
    w.failures.open = new ApiError(503, "storage", { code: "storage" });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.player.state.status).toBe("error");
    expect(w.audio.paused).toBe(true);
    expect(w.audio.src).toBe("");
    // Pressing play asks for the book again.
    w.failures.open = undefined;
    w.failures.sync = undefined;
    w.player.toggle();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.player.state.status).toBe("loading");
  });

  it("ignores the answer to a report from the book before", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.flags.finishedNow = true;
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced).toHaveLength(1);
    w.holds.sync = undefined;
    await w.player.play({ ...TARGET, editionId: "ed2" });
    w.audio.ready(600);
    w.clock.now += 1000;
    w.audio.at(1);
    release();
    await settle();
    expect(w.player.state.finished).toBe(false);
    w.flags.finishedNow = false;
    w.player.pause();
    await settle();
    expect(w.synced.at(-1)).toMatchObject({ id: "s2", report: { listened_ms: 1000 } });
  });

  it("keeps a report still in flight for one book out of the next book's close", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.sync = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced).toHaveLength(1);
    await w.player.play({ ...TARGET, editionId: "ed2" });
    w.audio.ready(600);
    w.clock.now += 1000;
    w.audio.at(1);
    await w.player.stop();
    expect(w.closed.at(-1)).toMatchObject({ id: "s2", report: { listened_ms: 1000 } });
  });

  it("does not hold a book's reports behind one still out for the book before", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.sync = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced.map((sent) => sent.id)).toEqual(["s1"]);
    w.holds.sync = undefined;
    await w.player.play({ ...TARGET, editionId: "ed2" });
    w.audio.ready(600);
    w.clock.now += SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced.map((sent) => sent.id)).toEqual(["s1", "s2"]);
  });

  it("sends a pause the ended session refused to the session that replaces it", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.clock.now = 5000;
    w.audio.at(5);
    w.failures.syncOnce = new ApiError(409, "session_ended", { code: "session_ended" });
    w.player.pause();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.player.state.status).toBe("paused");
    expect(w.synced).toEqual([
      { id: "s2", report: expect.objectContaining({ position_ms: 5000, listened_ms: 2000 }) },
    ]);
  });

  it("opens one replacement when the report and the stream both fail", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.open = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.failures.syncOnce = new ApiError(409, "session_ended", { code: "session_ended" });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    w.audio.emit("error");
    await settle();
    release();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    expect(w.closed).toEqual([]);
  });

  it("keeps a seek made while a replacement session opens", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.open = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.failures.syncOnce = new ApiError(409, "session_ended", { code: "session_ended" });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    w.player.seek(120_000);
    release();
    await settle();
    w.audio.ready(600);
    expect(w.audio.currentTime).toBe(120);
    expect(w.player.state.positionMs).toBe(120_000);
  });

  it("gives up a stream that fails twice, and play opens a new session", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.audio.at(15);
    w.audio.emit("error");
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    // The replacement fails before its metadata arrives.
    w.audio.emit("error");
    await settle();
    expect(w.player.state.status).toBe("error");
    expect(w.player.state.target).toEqual(TARGET);
    expect(w.audio.src).toBe("");
    expect(w.closed.map((sent) => sent.id)).toEqual(["s2"]);
    w.player.toggle();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1", "ed1"]);
    expect(w.audio.src).toBe("/api/playback/sessions/s3/tracks/0");
    // The server still says the start; the listener was fifteen seconds in.
    w.audio.ready(600);
    expect(w.audio.currentTime).toBe(15);
  });

  it("goes on from the listener's place when the new session cannot open at first", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.audio.at(15);
    w.audio.emit("error");
    await settle();
    w.audio.emit("error");
    await settle();
    w.failures.open = new ApiError(503, "storage", { code: "storage" });
    w.player.toggle();
    await settle();
    expect(w.player.state.status).toBe("error");
    w.failures.open = undefined;
    w.player.toggle();
    await settle();
    w.audio.ready(600);
    expect(w.audio.currentTime).toBe(15);
  });

  it("lets a stream that fails again while its replacement opens share that replacement", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.open = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.audio.at(15);
    w.audio.emit("error");
    await settle();
    w.audio.emit("error");
    await settle();
    expect(w.player.state.status).not.toBe("error");
    release();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    expect(w.closed).toEqual([]);
  });

  it("waits for a report on its way before closing, and carries its time when it fails", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.holds.wait = new Promise<void>(() => {});
    w.clock.now = 1000;
    w.audio.at(1);
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced[0].report.listened_ms).toBe(3000);
    w.clock.now += 1000;
    w.audio.at(16);
    const stopping = w.player.stop();
    await settle();
    expect(w.closed).toEqual([]);
    w.failures.syncLate = new Error("the connection dropped");
    release();
    await stopping;
    expect(w.closed[0].report).toMatchObject({ position_ms: 16_000, listened_ms: 4000 });
  });

  it("leaves the time of a report the server took out of the close that waited for it", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.holds.wait = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    w.clock.now += 1000;
    w.audio.at(16);
    const stopping = w.player.stop();
    await settle();
    expect(w.closed).toEqual([]);
    release();
    await stopping;
    expect(w.closed[0].report).toMatchObject({ position_ms: 16_000, listened_ms: 1000 });
  });

  it("waits for a close already on its way when stopped again", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.holds.wait = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    void w.player.stop();
    // What a sign-out asks for: nothing is loaded, but the close is not out yet.
    let done = false;
    const again = w.player.stop().then(() => {
      done = true;
    });
    await settle();
    expect(done).toBe(false);
    expect(w.closed).toEqual([]);
    release();
    await again;
    expect(w.closed.map((sent) => sent.id)).toEqual(["s1"]);
  });

  it("sends a close that was waiting at once when the page goes away", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.sync = new Promise<void>(() => {});
    w.holds.wait = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    void w.player.stop();
    await settle();
    expect(w.closed).toEqual([]);
    await w.player.stop(true);
    expect(w.closed).toHaveLength(1);
    expect(w.closed[0]).toMatchObject({ id: "s1", keepalive: true });
  });

  it("does not send a page that is going away waiting for a report", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.sync = new Promise<void>(() => {});
    w.holds.wait = new Promise<void>(() => {});
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    await w.player.stop(true);
    expect(w.closed).toHaveLength(1);
    expect(w.closed[0].keepalive).toBe(true);
  });

  it("does not count time twice when a replaced session's report is answered late", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced).toHaveLength(1);
    w.audio.emit("error");
    await settle();
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
    w.audio.ready(600);
    w.holds.sync = undefined;
    release();
    await settle();
    w.clock.now += 1000;
    w.audio.at(16);
    w.player.pause();
    await settle();
    expect(w.synced.at(-1)).toMatchObject({ id: "s2", report: { listened_ms: 1000 } });
  });

  it("pauses and resumes while a seek into another track loads", async () => {
    const w = world({ tracks: 2, lengths: [60_000, 60_000] });
    await w.player.play(TARGET);
    w.audio.ready(60);
    expect(w.player.state.status).toBe("playing");
    w.player.seek(90_000);
    // A browser pauses the element for the new source without an event.
    w.audio.paused = true;
    expect(w.player.state.status).toBe("loading");
    w.player.toggle();
    expect(w.player.state.status).toBe("paused");
    w.audio.ready(60);
    expect(w.audio.paused).toBe(true);
    w.player.toggle();
    expect(w.audio.paused).toBe(false);
    expect(w.player.state.status).toBe("playing");
    expect(w.audio.currentTime).toBe(30);
  });

  it("stays paused through a seek into another track after the browser paused it", async () => {
    const w = world({ tracks: 2, lengths: [60_000, 60_000] });
    await w.player.play(TARGET);
    w.audio.ready(60);
    expect(w.player.state.status).toBe("playing");
    // Not through the player: an interruption the browser reports.
    w.audio.pause();
    expect(w.player.state.status).toBe("paused");
    w.player.seek(90_000);
    w.audio.ready(60);
    expect(w.audio.paused).toBe(true);
    expect(w.player.state.status).toBe("paused");
    expect(w.audio.currentTime).toBe(30);
  });

  it("keeps a bookmark added before the list of bookmarks arrives", async () => {
    const w = world();
    let release = () => {};
    w.holds.manifest = new Promise<void>((resolve) => {
      release = resolve;
    });
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.audio.at(9);
    await w.player.addBookmark("the door");
    w.holds.manifest = undefined;
    release();
    await settle();
    expect(w.player.state.bookmarks.map((bookmark) => bookmark.id)).toEqual(["b1", "b9000"]);
  });

  it("reads the bookmarks again for as long as the listener keeps adding them", async () => {
    const w = world();
    let release = () => {};
    const gate = () => {
      w.holds.manifest = new Promise<void>((resolve) => {
        release = resolve;
      });
    };
    gate();
    await w.player.play(TARGET);
    w.audio.ready(600);
    for (const second of [9, 10, 11]) {
      w.audio.at(second);
      await w.player.addBookmark("");
      const answer = release;
      if (second === 11) w.holds.manifest = undefined;
      else gate();
      answer();
      await settle();
    }
    expect(w.player.state.bookmarks.map((bookmark) => bookmark.id)).toEqual([
      "b1",
      "b9000",
      "b10000",
      "b11000",
    ]);
  });

  it("closes a session whose tracks cannot be read", async () => {
    const w = world({ tracks: 2, lengths: [60_000, 60_000] });
    w.failures.probe = new Error("unreachable");
    await w.player.play(TARGET);
    expect(w.player.state.status).toBe("error");
    expect(w.closed.map((sent) => sent.id)).toEqual(["s1"]);
  });

  it("tells a caller that reads before it plays whether its choice still stands", async () => {
    const w = world();
    const first = w.player.claim();
    expect(first()).toBe(true);
    // A later choice replaces it, whichever read finishes first.
    const second = w.player.claim();
    expect(first()).toBe(false);
    expect(second()).toBe(true);
    await w.player.play(TARGET);
    expect(second()).toBe(false);
    const third = w.player.claim();
    await w.player.stop();
    expect(third()).toBe(false);
  });

  it("drops a pending choice when the listener goes back to the loaded book", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.player.pause();
    const other = w.player.claim();
    // Asking for the loaded book only resumes it, and is still the later choice.
    await w.player.play(TARGET);
    expect(w.opened).toEqual(["ed1"]);
    expect(other()).toBe(false);
  });

  it("lets a choice still being read stand when the loaded book's session fails", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    const choice = w.player.claim();
    w.audio.emit("error");
    await settle();
    w.audio.emit("error");
    await settle();
    expect(w.player.state.status).toBe("error");
    expect(choice()).toBe(true);
  });

  it("sends a report the ended session refused to a session a failed stream already opened", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    let release = () => {};
    w.holds.sync = new Promise<void>((resolve) => {
      release = resolve;
    });
    w.clock.now = SYNC_EVERY_MS;
    w.audio.at(15);
    await settle();
    expect(w.synced.map((sent) => sent.id)).toEqual(["s1"]);
    w.audio.emit("error");
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    w.holds.sync = undefined;
    w.failures.syncLate = new ApiError(409, "session_ended", { code: "session_ended" });
    release();
    await settle();
    w.failures.syncLate = undefined;
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
    expect(w.synced.at(-1)).toMatchObject({ id: "s2", report: { position_ms: 15_000 } });
  });

  it("starts the next book when the close of the one before never answers", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.holds.close = new Promise<void>(() => {});
    w.holds.switchWait = Promise.resolve();
    await w.player.play({ ...TARGET, editionId: "ed2" });
    expect(w.closed.map((sent) => sent.id)).toEqual(["s1"]);
    expect(w.opened).toEqual(["ed1", "ed2"]);
    expect(w.audio.src).toBe("/api/playback/sessions/s2/tracks/0");
  });

  it("opens a new session to go on from a place sought back to after the end", async () => {
    const w = world();
    w.flags.finishedNow = true;
    await w.player.play(TARGET);
    w.audio.ready(100);
    w.audio.at(100);
    w.audio.paused = true;
    w.audio.emit("ended");
    await settle();
    w.player.seek(40_000);
    w.player.resume();
    await settle();
    expect(w.closed.map((sent) => sent.id)).toEqual(["s1"]);
    expect(w.opened).toEqual(["ed1", "ed1"]);
    w.audio.ready(100);
    expect(w.audio.currentTime).toBe(40);
    // An ordinary pause and seek in the new session keeps it.
    w.player.pause();
    w.player.seek(20_000);
    w.player.resume();
    await settle();
    expect(w.opened).toEqual(["ed1", "ed1"]);
  });

  it("keeps the book and says so when it cannot be marked finished", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    w.failures.setProgress = new ApiError(503, "storage", { code: "storage" });
    await expect(w.player.markFinished()).rejects.toBeInstanceOf(ApiError);
    expect(w.player.state.status).toBe("error");
    expect(w.player.state.target).toEqual(TARGET);
    expect(w.player.state.error).toContain("could not be marked finished");
    // The action is still there to press again.
    w.failures.setProgress = undefined;
    await w.player.markFinished();
    expect(w.finished).toEqual(["ed1"]);
    expect(w.player.state.status).toBe("idle");
  });

  it("marks the book finished and unloads it", async () => {
    const w = world();
    await w.player.play(TARGET);
    w.audio.ready(600);
    await w.player.markFinished();
    expect(w.finished).toEqual(["ed1"]);
    expect(w.closed).toHaveLength(1);
    expect(w.player.state.status).toBe("idle");
  });

  it("ignores controls with nothing loaded", async () => {
    const w = world();
    w.player.seek(5000);
    w.player.resume();
    w.player.pause();
    await w.player.addBookmark("");
    await w.player.markFinished();
    w.audio.emit("timeupdate");
    expect(w.player.state.status).toBe("idle");
    expect(w.opened).toHaveLength(0);
  });
});

describe("playbackErrorText", () => {
  it("has words for each refusal", () => {
    const text = (status: number, code?: string) =>
      playbackErrorText(new ApiError(status, code ?? "x", { code }));
    expect(text(403)).toBe("Sign in as a library user to listen.");
    expect(text(404, "unknown_session")).toBe("This book is no longer in your library.");
    expect(text(421, "misdirected")).toContain("another server");
    expect(text(503, "unavailable")).toContain("storage");
    expect(text(500)).toBe("This book could not be played.");
  });
});
