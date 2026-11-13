import { describe, expect, it, vi } from "vitest";
import { LIVE_COMMAND, openDesktopLive, tauriBridge, type TauriBridge } from "@/lib/desktopLive";

/**
 * A bridge that records the callback and the invoke call.
 *
 * @param invoke - How `invoke` answers.
 * @returns The bridge, its registered callbacks, and a way to deliver to them.
 */
function fakeBridge(invoke: TauriBridge["invoke"] = async () => null) {
  const callbacks = new Map<number, (delivery: unknown) => void>();
  let nextId = 7;
  const bridge: TauriBridge = {
    invoke: vi.fn(invoke),
    transformCallback: (callback) => {
      const id = nextId++;
      callbacks.set(id, callback);
      return id;
    },
    unregisterCallback: vi.fn((id: number) => {
      callbacks.delete(id);
    }),
  };
  const deliver = (delivery: unknown) => {
    for (const callback of callbacks.values()) callback(delivery);
  };
  return { bridge, callbacks, deliver };
}

describe("tauriBridge", () => {
  it("finds the shell's bridge and nothing else", () => {
    const { bridge } = fakeBridge();
    expect(tauriBridge({ __TAURI_INTERNALS__: bridge })).toBe(bridge);
    expect(tauriBridge({})).toBeNull();
    expect(tauriBridge({ __TAURI_INTERNALS__: { invoke: () => null } })).toBeNull();
  });
});

describe("openDesktopLive", () => {
  it("starts the feed with a channel argument", () => {
    const { bridge } = fakeBridge();
    openDesktopLive(bridge);
    expect(bridge.invoke).toHaveBeenCalledTimes(1);
    const [command, args] = vi.mocked(bridge.invoke).mock.calls[0];
    expect(command).toBe(LIVE_COMMAND);
    expect(JSON.stringify(args)).toBe('{"onFrame":"__CHANNEL__:7"}');
  });

  it("delivers frames in order, holding early ones", () => {
    const { bridge, deliver } = fakeBridge();
    const socket = openDesktopLive(bridge);
    const seen: unknown[] = [];
    socket.onmessage = (event) => seen.push(event.data);
    deliver({ index: 1, message: "second" });
    expect(seen).toEqual([]);
    deliver({ index: 0, message: "first" });
    deliver({ index: 2, message: "third" });
    deliver({ index: 2, message: "repeat" });
    deliver({ index: "x", message: "junk" });
    deliver(null);
    expect(seen).toEqual(["first", "second", "third"]);
  });

  it("closes when the shell ends the feed, once every frame arrived", () => {
    const { bridge, callbacks, deliver } = fakeBridge();
    const socket = openDesktopLive(bridge);
    const seen: unknown[] = [];
    const closes: number[] = [];
    socket.onmessage = (event) => seen.push(event.data);
    socket.onclose = (event) => closes.push(event.code);
    deliver({ index: 1, end: true });
    expect(closes).toEqual([]);
    deliver({ index: 0, message: "last" });
    expect(seen).toEqual(["last"]);
    expect(closes).toEqual([1000]);
    expect(callbacks.size).toBe(0);
  });

  it("closes with 1011 when the shell refuses the command", async () => {
    const { bridge } = fakeBridge(async () => {
      throw new Error("not allowed");
    });
    const socket = openDesktopLive(bridge);
    const closed = new Promise<number>((resolve) => {
      socket.onclose = (event) => resolve(event.code);
    });
    socket.onerror = vi.fn();
    expect(await closed).toBe(1011);
    expect(socket.onerror).toHaveBeenCalled();
  });

  it("stops listening when the page closes it, without a close event", () => {
    const { bridge, callbacks, deliver } = fakeBridge();
    const socket = openDesktopLive(bridge);
    const onclose = vi.fn();
    const onmessage = vi.fn();
    socket.onclose = onclose;
    socket.onmessage = onmessage;
    socket.close(1000);
    deliver({ index: 0, message: "late" });
    expect(onmessage).not.toHaveBeenCalled();
    expect(onclose).not.toHaveBeenCalled();
    expect(callbacks.size).toBe(0);
    socket.close(1000);
    expect(bridge.unregisterCallback).toHaveBeenCalledTimes(1);
  });
});
