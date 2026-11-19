import assert from "node:assert/strict";
import { describe, it } from "vitest";
import {
  REPORT_TIMEOUT_MS,
  readyEvent,
  reportRun,
  runFields,
  stopEvent,
  type RunEvent,
} from "../src/usage.ts";

/** A session row as `finish` and `recordStartup` return it. */
const ROW = {
  run_id: "0e9ced79-df3c-4dae-872b-2e5772b06e69",
  started_at: 1_792_843_200,
  ends_at: 1_792_845_000,
  version: "abc1234",
  startup_seconds: 41,
};

describe("run events", () => {
  it("build from the session row", () => {
    assert.deepEqual(readyEvent(ROW), {
      kind: "ready",
      run_id: ROW.run_id,
      started_at: ROW.started_at,
      ends_at: ROW.ends_at,
      version: ROW.version,
      startup_seconds: 41,
    });
    assert.deepEqual(stopEvent(ROW, ROW.started_at + 600, "idle"), {
      kind: "stop",
      run_id: ROW.run_id,
      started_at: ROW.started_at,
      ends_at: ROW.ends_at,
      version: ROW.version,
      ended_at: ROW.started_at + 600,
      reason: "idle",
    });
    // A visitor who starts again ends the old run with "restart".
    assert.deepEqual(stopEvent(ROW, ROW.started_at + 60, "restart"), {
      kind: "stop",
      run_id: ROW.run_id,
      started_at: ROW.started_at,
      ends_at: ROW.ends_at,
      version: ROW.version,
      ended_at: ROW.started_at + 60,
      reason: "restart",
    });
  });

  it("are not built for rows without a run ID, odd versions or unknown reasons", () => {
    assert.equal(readyEvent(null), null);
    assert.equal(stopEvent(null, 1, "idle"), null);
    assert.equal(runFields({ ...ROW, run_id: "" }), null, "a row from before run IDs");
    assert.equal(runFields({ ...ROW, version: "v 1" }), null);
    assert.equal(stopEvent(ROW, ROW.started_at, "bogus"), null);
  });
});

describe("reportRun", () => {
  const event: RunEvent = { kind: "start", ...ROW };

  it("sends nothing without the binding", async () => {
    await reportRun({}, event);
  });

  it("never rejects when the collector fails", async () => {
    await reportRun(
      {
        USAGE: {
          record: async () => {
            throw new Error("down");
          },
        },
      },
      event,
    );
  });

  it("stops waiting after the timeout", async () => {
    const started = Date.now();
    await reportRun({ USAGE: { record: () => new Promise<boolean>(() => {}) } }, event);
    assert.ok(Date.now() - started < REPORT_TIMEOUT_MS + 1000);
  });
});
