import { afterEach, describe, expect, it, vi } from "vitest";
import type { TauriBridge } from "@/lib/desktopLive";
import {
  ALERTS_STORAGE_KEY,
  AlertTracker,
  NOTIFY_COMMAND,
  alertState,
  parseInboxPage,
  safeLinkPath,
  showAlert,
  storeAlerts,
  storedAlerts,
  type InboxEntryView,
} from "@/lib/notifications";

/**
 * An inbox entry.
 *
 * @param id - Its id.
 * @param read - Whether it was read.
 * @returns The entry.
 */
function entry(id: string, read = false): InboxEntryView {
  return {
    id,
    notification_type: "host:scan_failed",
    severity: "warning",
    title: `Title ${id}`,
    text: `Body ${id}`,
    link_path: "/library-status",
    fields: [],
    created_at: "2026-11-22T01:00:00+00:00",
    read_at: read ? "2026-11-22T01:05:00+00:00" : null,
  };
}

/**
 * Stubs a page `window` with working storage and focus.
 *
 * @returns The stubbed storage.
 */
function stubWindow(): Map<string, string> {
  const items = new Map<string, string>();
  vi.stubGlobal("window", {
    focus: vi.fn(),
    localStorage: {
      getItem: (key: string) => items.get(key) ?? null,
      setItem: (key: string, value: string) => void items.set(key, value),
      removeItem: (key: string) => void items.delete(key),
    },
  });
  return items;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("parseInboxPage", () => {
  it("keeps well-formed entries and drops the rest", () => {
    const page = parseInboxPage({
      entries: [
        { ...entry("a"), fields: [{ name: "Account", value: "libro-1" }, { name: 1 }] },
        { ...entry("b"), severity: "loud" },
        null,
        { ...entry("c"), link_path: 7, read_at: 3 },
      ],
      unread: 4,
      next_cursor: "abc",
    });
    expect(page.entries.map((e) => e.id)).toEqual(["a", "c"]);
    expect(page.entries[0].fields).toEqual([{ name: "Account", value: "libro-1" }]);
    expect(page.entries[1].link_path).toBeNull();
    expect(page.entries[1].read_at).toBeNull();
    expect(page.unread).toBe(4);
    expect(page.next_cursor).toBe("abc");
  });

  it("counts unread entries when the count is missing", () => {
    const page = parseInboxPage({ entries: [entry("a"), entry("b", true)], unread: -1 });
    expect(page.unread).toBe(1);
    expect(page.next_cursor).toBeNull();
    expect(parseInboxPage("junk")).toEqual({ entries: [], unread: 0, next_cursor: null });
  });
});

describe("safeLinkPath", () => {
  it("follows only paths inside the app", () => {
    expect(safeLinkPath("/library")).toBe("/library");
    expect(safeLinkPath(null)).toBeNull();
    expect(safeLinkPath("")).toBeNull();
    expect(safeLinkPath("//evil.example/x")).toBeNull();
    expect(safeLinkPath("/\\evil.example")).toBeNull();
    expect(safeLinkPath("https://evil.example")).toBeNull();
    expect(safeLinkPath("javascript:alert(1)")).toBeNull();
  });
});

describe("AlertTracker", () => {
  it("announces only unread entries that arrive after the first read", () => {
    const tracker = new AlertTracker();
    expect(tracker.observe([entry("a")])).toEqual([]);
    expect(tracker.observe([entry("c"), entry("b", true), entry("a")]).map((e) => e.id)).toEqual([
      "c",
    ]);
    expect(tracker.observe([entry("e"), entry("d"), entry("c")]).map((e) => e.id)).toEqual([
      "d",
      "e",
    ]);
    expect(tracker.observe([entry("e")])).toEqual([]);
    tracker.reset();
    expect(tracker.observe([entry("f")])).toEqual([]);
  });
});

describe("alertState", () => {
  it("follows the sink, the permission and the saved choice", () => {
    const bridge = {} as TauriBridge;
    const env = (
      sink: Parameters<typeof alertState>[0]["sink"],
      permission: NotificationPermission | null,
      stored: boolean,
    ) => alertState({ sink, permission: () => permission, stored: () => stored });
    expect(env({ kind: "none" }, "granted", true)).toBe("unsupported");
    expect(env({ kind: "desktop", bridge }, null, true)).toBe("on");
    expect(env({ kind: "desktop", bridge }, null, false)).toBe("off");
    expect(env({ kind: "browser" }, null, true)).toBe("unsupported");
    expect(env({ kind: "browser" }, "denied", true)).toBe("blocked");
    expect(env({ kind: "browser" }, "default", true)).toBe("off");
    expect(env({ kind: "browser" }, "granted", false)).toBe("off");
    expect(env({ kind: "browser" }, "granted", true)).toBe("on");
  });
});

describe("stored alerts", () => {
  it("round-trips this device's choice", () => {
    const items = stubWindow();
    expect(storedAlerts()).toBe(false);
    storeAlerts(true);
    expect(items.get(ALERTS_STORAGE_KEY)).toBe("on");
    expect(storedAlerts()).toBe(true);
    storeAlerts(false);
    expect(storedAlerts()).toBe(false);
  });

  it("reads off when storage is refused", () => {
    vi.stubGlobal("window", {
      localStorage: {
        getItem: () => {
          throw new Error("denied");
        },
        setItem: () => {
          throw new Error("denied");
        },
      },
    });
    expect(storedAlerts()).toBe(false);
    expect(() => storeAlerts(true)).not.toThrow();
  });
});

describe("showAlert", () => {
  it("asks the desktop shell for a native notification", async () => {
    const invoke = vi.fn(async () => null);
    const bridge = { invoke } as unknown as TauriBridge;
    await showAlert({ kind: "desktop", bridge }, entry("a"), () => undefined);
    expect(invoke).toHaveBeenCalledWith(NOTIFY_COMMAND, { title: "Title a", body: "Body a" });
  });

  it("swallows a shell refusal", async () => {
    const bridge = {
      invoke: vi.fn(async () => {
        throw new Error("not allowed");
      }),
    } as unknown as TauriBridge;
    await expect(
      showAlert({ kind: "desktop", bridge }, entry("a"), () => undefined),
    ).resolves.toBeUndefined();
  });

  it("shows a browser notification tagged with the id that follows its link", async () => {
    const made: { title: string; options: NotificationOptions; onclick: (() => void) | null }[] =
      [];
    class FakeNotification {
      onclick: (() => void) | null = null;
      constructor(title: string, options: NotificationOptions) {
        made.push(this as unknown as (typeof made)[number]);
        Object.assign(this, { title, options });
      }
      close() {}
    }
    vi.stubGlobal("Notification", FakeNotification);
    stubWindow();
    const clicked = vi.fn();
    await showAlert({ kind: "browser" }, entry("a"), clicked);
    expect(made).toHaveLength(1);
    expect(made[0].title).toBe("Title a");
    expect(made[0].options).toEqual({ body: "Body a", tag: "a" });
    made[0].onclick?.();
    expect(clicked).toHaveBeenCalledWith(entry("a"));
  });

  it("ignores a browser that refuses page notifications", async () => {
    vi.stubGlobal(
      "Notification",
      class {
        constructor() {
          throw new TypeError("Illegal constructor");
        }
      },
    );
    await expect(
      showAlert({ kind: "browser" }, entry("a"), () => undefined),
    ).resolves.toBeUndefined();
    await expect(showAlert({ kind: "none" }, entry("a"), () => undefined)).resolves.toBeUndefined();
  });
});
