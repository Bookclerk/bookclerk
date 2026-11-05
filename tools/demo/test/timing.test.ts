// Server-Timing entries on proxied answers.

import assert from "node:assert/strict";
import { describe, it } from "vitest";
import { timed, withServerTiming } from "../src/timing.ts";

describe("withServerTiming", () => {
  it("adds an entry and keeps the answer", async () => {
    const answer = new Response("ok", {
      status: 201,
      headers: { "Content-Type": "text/plain", "Server-Timing": "app;dur=3" },
    });
    const timed = withServerTiming(withServerTiming(answer, "container", 12.4), "session", -5);
    assert.equal(timed.status, 201);
    assert.equal(timed.headers.get("Content-Type"), "text/plain");
    assert.equal(timed.headers.get("Server-Timing"), "app;dur=3, container;dur=12, session;dur=0");
    assert.equal(await timed.text(), "ok");
  });

  it("times a call with the given clock", async () => {
    const ticks = [100, 142];
    const timedAnswer = await timed(
      "session",
      async () => new Response("ok"),
      () => ticks.shift() ?? 0,
    );
    assert.equal(timedAnswer.headers.get("Server-Timing"), "session;dur=42");
  });
});
