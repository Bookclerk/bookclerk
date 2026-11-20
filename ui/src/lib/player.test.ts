import { describe, expect, it } from "vitest";
import { ApiError } from "./api";
import type { Bookmark, PlaybackProgress, PlaybackReport, PlaybackSession } from "./playback";
import {
  Player,
  SKIP_MS,
  SYNC_EVERY_MS,
  playbackErrorText,
  type AudioLike,
  type PlayTarget,
  type PlayerDeps,
} from "./player";

// An audio element the tests move by hand.
class FakeAudio implements AudioLike {
  src = "";
  currentTime = 0;
  duration = Number.NaN;
  paused = true;
  playbackRate = 1;
  // What `play()` rejects with; `null` to start.
  refuse: unknown = null;
  private readonly listeners = new Map<string, (() => void)[]>();

  play(): Promise<void> {
    if (this.refuse) return Promise.reject(this.refuse);
    this.paused = false;
    this.emit("play");
    return Promise.resolve();
  }

  pause(): void {
    if (this.paused) return;
    this.paused = true;
    this.emit("pause");
  }

  addEventListener(type: string, listener: () => void): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }

  emit(type: string): void {
    for (const listener of this.listeners.get(type) ?? []) listener();
  }

  // The loaded file's metadata arrives.
  ready(seconds: number): void {
    this.duration = seconds;
    this.emit("loadedmetadata");
  }

  // Playback reaches `seconds`.
  at(seconds: number): void {
    this.currentTime = seconds;
    this.emit("timeupdate");
  }
}

const TARGET: PlayTarget = {
  editionId: "ed1",
  workId: "w1",
  title: "Piranesi",
  authors: "Susanna Clarke",
  coverPath: "/api/covers/abc",
};

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

// A player over fakes, with what it sent.
function world(options: { tracks?: number; startMs?: number; lengths?: number[] } = {}) {
  const audio = new FakeAudio();
  const clock = { now: 0 };
  const opened: string[] = [];
  const synced: { id: string; report: PlaybackReport }[] = [];
  const closed: { id: string; report: PlaybackReport; keepalive: boolean }[] = [];
  const finished: string[] = [];
  const saved: number[] = [];
  const failures: { open?: unknown; sync?: unknown; close?: unknown; manifest?: unknown } = {};
  // Requests wait on these before they answer.
  const holds: { open?: Promise<void>; sync?: Promise<void>; close?: Promise<void> } = {};
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
      return {
        edition_id: editionId,
        work_id: "w1",
        title: "Piranesi",
        playable: true,
        duration_ms: null,
        tracks: [],
        progress: null,
        bookmarks,
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
      if (failures.sync) throw failures.sync;
      synced.push({ id, report });
      await holds.sync;
      return { progress: progress(report.position_ms), finished_now: flags.finishedNow };
    },
    close: async (id, report, keepalive) => {
      closed.push({ id, report, keepalive });
      await holds.close;
      if (failures.close) throw failures.close;
      return { progress: progress(report.position_ms), finished_now: false };
    },
    addBookmark: async (editionId, positionMs, title) => ({
      id: `b${positionMs}`,
      edition_id: editionId,
      position_ms: positionMs,
      title,
      created_at: "2026-11-19T10:00:00Z",
    }),
    deleteBookmark: async () => undefined,
    setProgress: async (editionId) => {
      finished.push(editionId);
      return { progress: progress(0, true), finished_now: true };
    },
    probe: async (path) => (options.lengths ?? [])[Number(path.split("/").pop())] ?? 0,
    now: () => clock.now,
    loadSpeed: () => "1.5",
    saveSpeed: (speed) => saved.push(speed),
  };
  const player = new Player(deps);
  return { player, audio, clock, opened, synced, closed, finished, saved, failures, flags, holds };
}

// Lets resolved promises run.
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

  it("counts loads and unloads for callers that read before they play", async () => {
    const w = world();
    const before = w.player.loads;
    await w.player.play(TARGET);
    const loaded = w.player.loads;
    expect(loaded).not.toBe(before);
    await w.player.stop();
    expect(w.player.loads).not.toBe(loaded);
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
