import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  BACKOFF_MAX_MS,
  BACKOFF_MIN_MS,
  CLOSE_UNAUTHORIZED,
  LIVE_SUBPROTOCOL,
  LiveClient,
  backoffDelay,
  liveUrl,
  watchLive,
  type LiveSocket,
} from "./live";

/** A socket the test drives by hand. */
class FakeSocket implements LiveSocket {
  onmessage: LiveSocket["onmessage"] = null;
  onclose: LiveSocket["onclose"] = null;
  onerror: LiveSocket["onerror"] = null;
  closedWith: number | undefined;
  readonly url: string;
  readonly protocol: string;

  constructor(url: string, protocol: string) {
    this.url = url;
    this.protocol = protocol;
  }

  close(code?: number) {
    this.closedWith = code ?? 1000;
  }

  send(frame: unknown) {
    this.onmessage?.({ data: typeof frame === "string" ? frame : JSON.stringify(frame) });
  }

  hello(topics: string[]) {
    this.send({ v: 1, type: "hello", topics });
  }

  changed(topics: string[]) {
    this.send({ v: 1, type: "changed", topics });
  }

  drop(code = 1006) {
    this.onclose?.({ code });
  }
}

function harness(protocol = "http:") {
  const sockets: FakeSocket[] = [];
  const client = new LiveClient({
    location: { protocol, host: "127.0.0.1:8787" },
    connect: (url, sub) => {
      const socket = new FakeSocket(url, sub);
      sockets.push(socket);
      return socket;
    },
    random: () => 0,
  });
  return { client, sockets, last: () => sockets[sockets.length - 1] };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("liveUrl", () => {
  it("follows the page scheme on the page host", () => {
    expect(liveUrl({ protocol: "http:", host: "127.0.0.1:8787" })).toBe(
      "ws://127.0.0.1:8787/api/live",
    );
    expect(liveUrl({ protocol: "https:", host: "books.example" })).toBe(
      "wss://books.example/api/live",
    );
  });

  it("asks for the live subprotocol", () => {
    const { client, last } = harness("https:");
    client.subscribe(["jobs"], () => {});
    expect(last().url).toBe("wss://127.0.0.1:8787/api/live");
    expect(last().protocol).toBe(LIVE_SUBPROTOCOL);
  });
});

describe("backoffDelay", () => {
  it("doubles up to the ceiling, jittered within its upper half", () => {
    expect(backoffDelay(0, 0)).toBe(BACKOFF_MIN_MS / 2);
    expect(backoffDelay(0, 0.999)).toBeLessThanOrEqual(BACKOFF_MIN_MS);
    expect(backoffDelay(1, 0)).toBe(BACKOFF_MIN_MS);
    expect(backoffDelay(3, 0)).toBe(4 * BACKOFF_MIN_MS);
    expect(backoffDelay(20, 0)).toBe(BACKOFF_MAX_MS / 2);
    expect(backoffDelay(20, 0.999)).toBeLessThanOrEqual(BACKOFF_MAX_MS);
  });
});

describe("LiveClient", () => {
  it("dispatches each subscriber only its own topics", () => {
    const { client, sockets, last } = harness();
    const jobs = vi.fn();
    const nodes = vi.fn();
    client.subscribe(["jobs", "deliveries"], jobs);
    client.subscribe(["nodes"], nodes);
    expect(sockets).toHaveLength(1);
    last().hello(["jobs", "deliveries", "nodes"]);
    expect(client.state).toBe("open");
    expect(jobs).not.toHaveBeenCalled();

    last().changed(["jobs", "consents"]);
    expect(jobs).toHaveBeenCalledWith(["jobs"]);
    expect(nodes).not.toHaveBeenCalled();
    last().changed(["nodes"]);
    expect(nodes).toHaveBeenCalledTimes(1);
    expect(jobs).toHaveBeenCalledTimes(1);

    // Junk frames are ignored.
    last().send("not json");
    last().send({ v: 2, type: "changed", topics: ["jobs"] });
    last().send({ v: 1, type: "rows", topics: ["jobs"] });
    expect(jobs).toHaveBeenCalledTimes(1);
  });

  it("reconnects with backoff and replays the hello topics after a drop", () => {
    const { client, sockets, last } = harness();
    const onChange = vi.fn();
    client.subscribe(["jobs"], onChange);
    last().hello(["jobs"]);
    last().drop();
    expect(client.state).toBe("closed");

    vi.advanceTimersByTime(backoffDelay(0, 0) - 1);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(2);
    // Refused again: the next wait doubles.
    last().drop();
    vi.advanceTimersByTime(backoffDelay(1, 0) - 1);
    expect(sockets).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(3);

    // Whatever changed while down was never announced: refresh once.
    last().hello(["jobs"]);
    expect(onChange).toHaveBeenCalledWith(["jobs"]);
    // A good connection resets the backoff.
    last().drop();
    vi.advanceTimersByTime(backoffDelay(0, 0));
    expect(sockets).toHaveLength(4);
  });

  it("waits the longest delay after a sign-out close", () => {
    const { client, sockets, last } = harness();
    client.subscribe(["nodes"], () => {});
    last().hello(["nodes"]);
    last().drop(CLOSE_UNAUTHORIZED);
    vi.advanceTimersByTime(BACKOFF_MAX_MS - 1);
    expect(sockets).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(sockets).toHaveLength(2);
  });

  it("closes when the last subscriber leaves and stops retrying", () => {
    const { client, sockets, last } = harness();
    const first = client.subscribe(["jobs"], () => {});
    const second = client.subscribe(["nodes"], () => {});
    last().hello(["jobs", "nodes"]);
    first();
    expect(last().closedWith).toBeUndefined();
    second();
    expect(last().closedWith).toBe(1000);
    expect(client.state).toBe("idle");
    vi.advanceTimersByTime(BACKOFF_MAX_MS * 2);
    expect(sockets).toHaveLength(1);
  });
});

describe("watchLive", () => {
  it("polls while the socket is down and refreshes on its topics while up", () => {
    const { client, last } = harness();
    const refresh = vi.fn();
    const stop = watchLive(client, ["nodes"], refresh, { pollMs: 15_000 });

    // Connecting: the fallback timer runs.
    vi.advanceTimersByTime(15_000);
    expect(refresh).toHaveBeenCalledTimes(1);

    // Open: no timer, only topic changes.
    last().hello(["nodes"]);
    vi.advanceTimersByTime(60_000);
    expect(refresh).toHaveBeenCalledTimes(1);
    last().changed(["jobs"]);
    expect(refresh).toHaveBeenCalledTimes(1);
    last().changed(["nodes"]);
    expect(refresh).toHaveBeenCalledTimes(2);

    // Down again: polling resumes.
    last().drop();
    vi.advanceTimersByTime(15_000);
    expect(refresh).toHaveBeenCalledTimes(3);

    stop();
    vi.advanceTimersByTime(120_000);
    expect(refresh).toHaveBeenCalledTimes(3);
  });

  it("keeps a slow poll while open when asked", () => {
    const { client, last } = harness();
    const refresh = vi.fn();
    watchLive(client, ["users"], refresh, { pollMs: 5_000, openPollMs: 30_000 });
    last().hello(["users"]);
    vi.advanceTimersByTime(29_999);
    expect(refresh).not.toHaveBeenCalled();
    vi.advanceTimersByTime(1);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it("never polls with a zero interval, and refreshes on change and reconnect", () => {
    const { client, last } = harness();
    const refresh = vi.fn();
    watchLive(client, ["library"], refresh, { pollMs: 0 });
    vi.advanceTimersByTime(120_000);
    expect(refresh).not.toHaveBeenCalled();
    last().hello(["library", "nodes"]);
    expect(refresh).not.toHaveBeenCalled();
    last().changed(["library"]);
    expect(refresh).toHaveBeenCalledTimes(1);
    last().drop();
    vi.advanceTimersByTime(120_000);
    expect(refresh).toHaveBeenCalledTimes(1);
    last().hello(["library", "nodes"]);
    expect(refresh).toHaveBeenCalledTimes(2);
  });

  it("refreshes the clusters panel on cluster link changes", () => {
    const { client, last } = harness();
    const refresh = vi.fn();
    watchLive(client, ["consents", "clusters"], refresh, { pollMs: 15_000 });
    last().hello(["consents", "clusters"]);
    last().changed(["clusters"]);
    expect(refresh).toHaveBeenCalledTimes(1);
  });
});
