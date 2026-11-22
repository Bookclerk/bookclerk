/**
 * The signed-in person's notification inbox, and the alerts this device
 * shows for it.
 *
 * The library's router decides what reaches an inbox (a patron type the
 * person gets, or an operational type an administrator routed to "In app"
 * for Owners and Administrators). The web app reads the inbox on the
 * `notifications` live topic and shows it behind the bell. Where the person
 * turned alerts on for this device, a new unread entry also becomes a
 * system notification: the browser's Notification API on the web, or the
 * desktop window shell's `notify` command, which posts a native one. Alerts
 * only follow the inbox, so they never show anything the router did not
 * put there.
 */

import type { TauriBridge } from "@/lib/desktopLive";

/** Shell command that posts a native notification in the desktop window. */
export const NOTIFY_COMMAND = "notify";

/** Where this device remembers that alerts are on. */
export const ALERTS_STORAGE_KEY = "bookclerk.notifications.alerts";

/** How urgent a notification is. */
export type NotificationSeverity = "info" | "warning" | "critical";

/** One labelled value on a notification. */
export interface NotificationFieldView {
  /** Label. */
  name: string;
  /** Value. */
  value: string;
}

/** One inbox entry, as `GET /api/me/notifications/inbox` returns it. */
export interface InboxEntryView {
  /** Notification id. */
  id: string;
  /** Type id, such as `discover:new_book`. */
  notification_type: string;
  /** How urgent it is. */
  severity: NotificationSeverity;
  /** Title. */
  title: string;
  /** Plain-text body. */
  text: string;
  /** Path inside the library it links to. */
  link_path: string | null;
  /** Labelled values. */
  fields: NotificationFieldView[];
  /** When it arrived (RFC 3339). */
  created_at: string;
  /** When it was read (RFC 3339); `null` while unread. */
  read_at: string | null;
}

/** One page of the inbox. */
export interface InboxPageView {
  /** Newest first. */
  entries: InboxEntryView[];
  /** Unread entries in the whole inbox. */
  unread: number;
  /** Cursor of the next page; `null` on the last. */
  next_cursor: string | null;
}

/** Who shows this device's alerts. */
export type AlertSink =
  { kind: "desktop"; bridge: TauriBridge } | { kind: "browser" } | { kind: "none" };

/** Whether alerts can be turned on here, and whether they are. */
export type AlertState =
  /** Neither the browser nor the shell can show one. */
  | "unsupported"
  /** The browser refused permission; only its site settings can undo it. */
  | "blocked"
  /** Available, and off on this device. */
  | "off"
  /** On. */
  | "on";

/**
 * Reads one inbox entry from untrusted JSON.
 *
 * @param raw - One element of `entries`.
 * @returns The entry, or `null` when a required field is missing or mistyped.
 */
export function parseInboxEntry(raw: unknown): InboxEntryView | null {
  if (typeof raw !== "object" || raw === null) return null;
  const body = raw as Record<string, unknown>;
  const severity = body.severity;
  if (
    typeof body.id !== "string" ||
    typeof body.notification_type !== "string" ||
    (severity !== "info" && severity !== "warning" && severity !== "critical") ||
    typeof body.title !== "string" ||
    typeof body.text !== "string" ||
    typeof body.created_at !== "string"
  ) {
    return null;
  }
  const fields = Array.isArray(body.fields)
    ? body.fields.flatMap((field: unknown) => {
        if (typeof field !== "object" || field === null) return [];
        const { name, value } = field as { name?: unknown; value?: unknown };
        return typeof name === "string" && typeof value === "string" ? [{ name, value }] : [];
      })
    : [];
  return {
    id: body.id,
    notification_type: body.notification_type,
    severity,
    title: body.title,
    text: body.text,
    link_path: typeof body.link_path === "string" ? body.link_path : null,
    fields,
    created_at: body.created_at,
    read_at: typeof body.read_at === "string" ? body.read_at : null,
  };
}

/**
 * Reads an inbox page from untrusted JSON, dropping malformed entries.
 *
 * @param raw - The response body.
 * @returns The page.
 */
export function parseInboxPage(raw: unknown): InboxPageView {
  const body = (typeof raw === "object" && raw !== null ? raw : {}) as Record<string, unknown>;
  const entries = Array.isArray(body.entries)
    ? body.entries.map(parseInboxEntry).filter((e): e is InboxEntryView => e !== null)
    : [];
  const unread =
    typeof body.unread === "number" && Number.isInteger(body.unread) && body.unread >= 0
      ? body.unread
      : entries.filter((e) => e.read_at === null).length;
  return {
    entries,
    unread,
    next_cursor: typeof body.next_cursor === "string" ? body.next_cursor : null,
  };
}

/**
 * A notification's link as a path inside this app, or `null` when it is
 * not one: only a single-slash absolute path is followed, never another
 * origin or a scheme.
 *
 * @param linkPath - The entry's `link_path`.
 * @returns The path, or `null`.
 */
export function safeLinkPath(linkPath: string | null): string | null {
  if (!linkPath || !linkPath.startsWith("/") || linkPath.startsWith("//")) return null;
  if (linkPath.includes("\\")) return null;
  return linkPath;
}

/**
 * Tracks which inbox entries this page has already seen, so only entries
 * that arrive while it is open raise an alert. The first read is the
 * baseline: what was already there when the page loaded is not announced.
 */
export class AlertTracker {
  private seen: Set<string> | null = null;

  /**
   * Records a fresh read of the inbox's newest entries.
   *
   * @param entries - The newest page, newest first.
   * @returns The unread entries not seen before, oldest first; none on the
   *   first read.
   */
  observe(entries: readonly InboxEntryView[]): InboxEntryView[] {
    const first = this.seen === null;
    const seen = this.seen ?? new Set<string>();
    const fresh = entries.filter((e) => !seen.has(e.id) && e.read_at === null);
    for (const entry of entries) seen.add(entry.id);
    this.seen = seen;
    return first ? [] : fresh.reverse();
  }

  /** Forgets everything, so the next read is a new baseline (after a sign-out). */
  reset(): void {
    this.seen = null;
  }
}

/** What {@link alertState} and {@link showAlert} read from the page. */
export interface AlertEnv {
  /** Where alerts go. */
  sink: AlertSink;
  /** The browser's permission, when it has the Notification API. */
  permission(): NotificationPermission | null;
  /** This device's saved choice. */
  stored(): boolean;
}

/**
 * Whether alerts are available here and on.
 *
 * @param env - The page's sink, permission and saved choice.
 * @returns The state.
 */
export function alertState(env: AlertEnv): AlertState {
  switch (env.sink.kind) {
    case "none":
      return "unsupported";
    case "desktop":
      return env.stored() ? "on" : "off";
    case "browser": {
      const permission = env.permission();
      if (permission === null) return "unsupported";
      if (permission === "denied") return "blocked";
      return env.stored() && permission === "granted" ? "on" : "off";
    }
  }
}

/** What a shown alert does when clicked. */
export type AlertClick = (entry: InboxEntryView) => void;

/**
 * Shows one alert for `entry` on `sink`.
 *
 * @param sink - Where it goes.
 * @param entry - The new inbox entry.
 * @param onClick - Called when a browser alert is clicked. A desktop alert
 *   carries no click action: the system shows it, and the bell holds the
 *   entry when the person comes back to the window.
 * @returns Resolves once it was handed over; a refusal is swallowed, since
 *   the bell still shows the entry.
 */
export async function showAlert(
  sink: AlertSink,
  entry: InboxEntryView,
  onClick: AlertClick,
): Promise<void> {
  switch (sink.kind) {
    case "none":
      return;
    case "desktop":
      await sink.bridge
        .invoke(NOTIFY_COMMAND, { title: entry.title, body: entry.text })
        .catch(() => undefined);
      return;
    case "browser": {
      try {
        // The tag is the notification id, so several open tabs of the app
        // replace each other's alert rather than stacking copies.
        const alert = new Notification(entry.title, { body: entry.text, tag: entry.id });
        alert.onclick = () => {
          window.focus();
          alert.close();
          onClick(entry);
        };
      } catch {
        // Some browsers allow notifications only from a service worker.
      }
      return;
    }
  }
}

/**
 * Reads this device's saved choice.
 *
 * @returns Whether alerts were turned on here.
 */
export function storedAlerts(): boolean {
  try {
    return window.localStorage.getItem(ALERTS_STORAGE_KEY) === "on";
  } catch {
    return false;
  }
}

/**
 * Saves this device's choice.
 *
 * @param on - Whether alerts are on.
 */
export function storeAlerts(on: boolean): void {
  try {
    if (on) window.localStorage.setItem(ALERTS_STORAGE_KEY, "on");
    else window.localStorage.removeItem(ALERTS_STORAGE_KEY);
  } catch {
    // Private windows may refuse storage; the choice lasts this page only.
  }
}

/**
 * The browser's notification permission.
 *
 * @returns The permission, or `null` without the Notification API.
 */
export function browserPermission(): NotificationPermission | null {
  return typeof Notification === "undefined" ? null : Notification.permission;
}
