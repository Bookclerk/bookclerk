import { afterEach, describe, expect, it, vi } from "vitest";
import { isApiError } from "./api";
import {
  addBookmark,
  closePlaybackSession,
  deleteBookmark,
  deletePlaybackProgress,
  fetchPlaybackManifest,
  fetchPlaybackProgress,
  openPlaybackSession,
  putPlaybackProgress,
  syncPlaybackSession,
} from "./playback";

// Stubs `fetch` with one answer and records the requests.
function stubFetch(status: number, body: unknown) {
  const calls: { url: string; init: RequestInit }[] = [];
  vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
    calls.push({ url, init });
    const text = typeof body === "string" ? body : JSON.stringify(body);
    return new Response(status === 204 ? null : text, { status });
  });
  return calls;
}

afterEach(() => vi.unstubAllGlobals());

describe("playback routes", () => {
  it("asks each route the way the daemon expects", async () => {
    const calls = stubFetch(200, {});
    await fetchPlaybackManifest("ed/1");
    await openPlaybackSession("ed1", "Web (Firefox)");
    await syncPlaybackSession("s1", { position_ms: 5 });
    await closePlaybackSession("s1", { position_ms: 9 }, true);
    await fetchPlaybackProgress();
    await fetchPlaybackProgress({ limit: 6, cursor: "c 1", includeFinished: true });
    await putPlaybackProgress("ed1", { is_finished: true });
    await addBookmark("ed1", 1234.6, "here");
    expect(calls.map((call) => `${call.init.method} ${call.url}`)).toEqual([
      "GET /api/library/editions/ed%2F1/playback",
      "POST /api/playback/sessions",
      "POST /api/playback/sessions/s1/sync",
      "POST /api/playback/sessions/s1/close",
      "GET /api/playback/progress",
      "GET /api/playback/progress?limit=6&cursor=c+1&include_finished=true",
      "PUT /api/playback/progress/ed1",
      "POST /api/library/editions/ed1/bookmarks",
    ]);
    expect(JSON.parse(String(calls[1].init.body))).toEqual({
      edition_id: "ed1",
      player: "web",
      player_name: "Web (Firefox)",
    });
    expect(calls[0].init.headers).toBeUndefined();
    expect(calls[2].init.headers).toEqual({ "Content-Type": "application/json" });
    expect(calls[2].init.keepalive).toBe(false);
    expect(calls[3].init.keepalive).toBe(true);
    expect(JSON.parse(String(calls[7].init.body))).toEqual({ position_ms: 1235, title: "here" });
    expect(calls.every((call) => call.init.credentials === "include")).toBe(true);
  });

  it("answers nothing for a delete", async () => {
    const calls = stubFetch(204, null);
    await expect(deletePlaybackProgress("ed1")).resolves.toBeUndefined();
    await expect(deleteBookmark("b1")).resolves.toBeUndefined();
    expect(calls.map((call) => `${call.init.method} ${call.url}`)).toEqual([
      "DELETE /api/playback/progress/ed1",
      "DELETE /api/playback/bookmarks/b1",
    ]);
  });

  it("throws the daemon's error slug", async () => {
    stubFetch(409, { error: "session_ended" });
    const err = await syncPlaybackSession("s1", { position_ms: 1 }).catch((e: unknown) => e);
    expect(isApiError(err) && [err.status, err.code]).toEqual([409, "session_ended"]);
  });

  it("throws the status when the body is not the daemon's", async () => {
    stubFetch(502, "<html>bad gateway</html>");
    const err = await fetchPlaybackManifest("ed1").catch((e: unknown) => e);
    expect(isApiError(err) && [err.status, err.code]).toEqual([502, undefined]);
    stubFetch(500, { error: 7 });
    const other = await fetchPlaybackManifest("ed1").catch((e: unknown) => e);
    expect(isApiError(other) && other.code).toBeUndefined();
  });
});
