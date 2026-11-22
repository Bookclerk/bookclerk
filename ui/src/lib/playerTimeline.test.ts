import { describe, expect, it } from "vitest";
import {
  SPEEDS,
  chapterAt,
  formatClock,
  formatRemaining,
  locate,
  nearestSpeed,
  nextChapterStart,
  previousChapterStart,
  speedLabel,
  trackStarts,
} from "./playerTimeline";

describe("trackStarts", () => {
  it("starts each track where the one before ends", () => {
    expect(trackStarts([1000, 2500, 400])).toEqual([0, 1000, 3500]);
    expect(trackStarts([])).toEqual([]);
  });

  it("counts a nonsense length as nothing", () => {
    expect(trackStarts([1000, -5, 300])).toEqual([0, 1000, 1000]);
  });
});

describe("locate", () => {
  const tracks = [1000, 2000, 3000];

  it("finds the track a position falls in", () => {
    expect(locate(tracks, 0)).toEqual({ index: 0, offsetMs: 0 });
    expect(locate(tracks, 999)).toEqual({ index: 0, offsetMs: 999 });
    expect(locate(tracks, 1000)).toEqual({ index: 1, offsetMs: 0 });
    expect(locate(tracks, 4200)).toEqual({ index: 2, offsetMs: 1200 });
  });

  it("clamps to the start and the end", () => {
    expect(locate(tracks, -40)).toEqual({ index: 0, offsetMs: 0 });
    expect(locate(tracks, 99_000)).toEqual({ index: 2, offsetMs: 3000 });
  });

  it("treats one file of unknown length as the whole timeline", () => {
    expect(locate([Number.POSITIVE_INFINITY], 75_000)).toEqual({ index: 0, offsetMs: 75_000 });
  });

  it("answers the start for no tracks", () => {
    expect(locate([], 500)).toEqual({ index: 0, offsetMs: 0 });
  });
});

describe("formatClock", () => {
  it("shows hours only when there are some", () => {
    expect(formatClock(0)).toBe("0:00");
    expect(formatClock(123_000)).toBe("2:03");
    expect(formatClock(3_723_000)).toBe("1:02:03");
  });

  it("reads nonsense as zero", () => {
    expect(formatClock(-1)).toBe("0:00");
    expect(formatClock(Number.NaN)).toBe("0:00");
  });
});

describe("formatRemaining", () => {
  it("divides what is left by the speed", () => {
    expect(formatRemaining(3_600_000, 1)).toBe("1 h 0 min");
    expect(formatRemaining(3_600_000, 2)).toBe("30 min");
    expect(formatRemaining(17_520_000, 1)).toBe("4 h 52 min");
  });

  it("says so under a minute", () => {
    expect(formatRemaining(40_000, 1)).toBe("less than a minute");
    expect(formatRemaining(-5, 1)).toBe("less than a minute");
  });
});

describe("speeds", () => {
  it("snaps a stored value to an offered speed", () => {
    expect(nearestSpeed("1.24")).toBe(1.2);
    expect(nearestSpeed(9)).toBe(3);
    expect(nearestSpeed(0.1)).toBe(0.75);
    for (const speed of SPEEDS) expect(nearestSpeed(speed)).toBe(speed);
  });

  it("falls back to normal speed", () => {
    expect(nearestSpeed(null)).toBe(1);
    expect(nearestSpeed("fast")).toBe(1);
    expect(nearestSpeed(-2)).toBe(1);
  });

  it("labels a speed without trailing zeros", () => {
    expect(speedLabel(1)).toBe("1×");
    expect(speedLabel(1.2)).toBe("1.2×");
    expect(speedLabel(1.75)).toBe("1.75×");
  });
});

describe("chapters", () => {
  // A part holding two chapters, then a second part.
  const chapters = [
    { start_ms: 0, end_ms: 0 },
    { start_ms: 0, end_ms: 600_000 },
    { start_ms: 600_000, end_ms: 1_800_000 },
    { start_ms: 1_800_000, end_ms: 3_600_000 },
  ];

  it("finds the innermost chapter playing", () => {
    expect(chapterAt(chapters, 0)).toBe(1);
    expect(chapterAt(chapters, 599_999)).toBe(1);
    expect(chapterAt(chapters, 600_000)).toBe(2);
    expect(chapterAt(chapters, 9_999_999)).toBe(3);
    expect(chapterAt([], 5)).toBe(-1);
    expect(chapterAt([{ start_ms: 10, end_ms: 20 }], 5)).toBe(-1);
  });

  it("goes back to this chapter's start, or the one before near it", () => {
    expect(previousChapterStart(chapters, 700_000)).toBe(600_000);
    expect(previousChapterStart(chapters, 601_000)).toBe(0);
    expect(previousChapterStart(chapters, 1_000)).toBe(0);
    expect(previousChapterStart([], 50_000)).toBe(0);
  });

  it("goes forward to the next start, or nowhere in the last", () => {
    expect(nextChapterStart(chapters, 0)).toBe(600_000);
    expect(nextChapterStart(chapters, 700_000)).toBe(1_800_000);
    expect(nextChapterStart(chapters, 1_800_000)).toBeNull();
  });
});
