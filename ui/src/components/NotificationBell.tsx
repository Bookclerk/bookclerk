import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { Bell, BellOff, BellRing, CheckCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { fetchInbox, markInboxRead, type AppView } from "@/lib/api";
import { isDesktopWindow } from "@/lib/desktop";
import { tauriBridge } from "@/lib/desktopLive";
import { useLiveRefresh } from "@/lib/live";
import {
  AlertTracker,
  alertState,
  browserPermission,
  safeLinkPath,
  showAlert,
  storeAlerts,
  storedAlerts,
  type AlertSink,
  type AlertState,
  type InboxEntryView,
} from "@/lib/notifications";
import { viewFromPath } from "@/lib/routes";
import { cn } from "@/lib/utils";

/** Entries the bell shows. */
const BELL_PAGE = 20;

/** Inbox re-read while the live socket is down. */
const POLL_MS = 60_000;

/**
 * Where this page's alerts go: the desktop shell, the browser, or nowhere.
 *
 * @returns The sink.
 */
function pageSink(): AlertSink {
  const bridge = isDesktopWindow() ? tauriBridge() : null;
  if (bridge) return { kind: "desktop", bridge };
  if (browserPermission() !== null) return { kind: "browser" };
  return { kind: "none" };
}

/**
 * When an entry arrived, for its row.
 *
 * @param iso - RFC 3339 time.
 * @returns A short local date and time.
 */
function arrived(iso: string): string {
  const when = new Date(iso);
  if (Number.isNaN(when.getTime())) return "";
  return when.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
}

/**
 * The bell in the top bar: the signed-in person's notification inbox, and
 * the switch that turns this device's alerts (browser or desktop system
 * notifications) on or off.
 *
 * @param props - `onNavigate` opens a notification's page.
 */
export function NotificationBell({ onNavigate }: { onNavigate: (view: AppView) => void }) {
  const [open, setOpen] = useState(false);
  const [entries, setEntries] = useState<InboxEntryView[]>([]);
  const [unread, setUnread] = useState(0);
  const [failed, setFailed] = useState(false);
  const panelId = useId();
  const root = useRef<HTMLDivElement>(null);
  const tracker = useRef(new AlertTracker());
  const sink = useMemo(() => pageSink(), []);
  const [alerts, setAlerts] = useState<AlertState>(() =>
    alertState({ sink, permission: browserPermission, stored: storedAlerts }),
  );
  const alertsRef = useRef(alerts);
  useEffect(() => {
    alertsRef.current = alerts;
  }, [alerts]);

  const follow = useCallback(
    (entry: InboxEntryView) => {
      const path = safeLinkPath(entry.link_path);
      const view = path ? viewFromPath(path) : null;
      if (view) onNavigate(view);
    },
    [onNavigate],
  );

  const refresh = useCallback(async () => {
    try {
      const page = await fetchInbox({ limit: BELL_PAGE });
      setEntries(page.entries);
      setUnread(page.unread);
      setFailed(false);
      const fresh = tracker.current.observe(page.entries);
      // The bell is enough while the person is looking at the app.
      if (alertsRef.current === "on" && !document.hasFocus()) {
        for (const entry of fresh) void showAlert(sink, entry, follow);
      }
    } catch {
      setFailed(true);
    }
  }, [sink, follow]);

  useLiveRefresh(["notifications"], refresh, { pollMs: POLL_MS });

  useEffect(() => {
    if (!open) return;
    const onPointerDown = (e: PointerEvent) => {
      if (root.current && !root.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  async function markRead(id: string | null) {
    try {
      await markInboxRead(id);
    } catch {
      // The next refresh shows what stuck.
    }
    await refresh();
  }

  async function turnAlertsOn() {
    if (sink.kind === "browser") {
      const permission = await Notification.requestPermission().catch(() => "default" as const);
      if (permission !== "granted") {
        setAlerts(permission === "denied" ? "blocked" : "off");
        return;
      }
    }
    storeAlerts(true);
    setAlerts("on");
  }

  function turnAlertsOff() {
    storeAlerts(false);
    setAlerts(alertState({ sink, permission: browserPermission, stored: () => false }));
  }

  const label = unread > 0 ? `Notifications, ${unread} unread` : "Notifications";
  const where = sink.kind === "desktop" ? "system notifications" : "browser notifications";

  return (
    <div className="relative" ref={root}>
      <Button
        variant="ghost"
        onClick={() => setOpen((o) => !o)}
        aria-label={label}
        title={label}
        aria-haspopup="dialog"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        className="relative"
      >
        {unread > 0 ? <BellRing className="h-4 w-4" /> : <Bell className="h-4 w-4" />}
        {unread > 0 ? (
          <span
            aria-hidden="true"
            className="absolute -right-0.5 -top-0.5 min-w-[1.1rem] rounded-full bg-teal px-1 text-center text-[10px] font-semibold leading-[1.1rem] text-paper"
          >
            {unread > 99 ? "99+" : unread}
          </span>
        ) : null}
      </Button>
      {open ? (
        <div
          id={panelId}
          role="dialog"
          aria-label="Notifications"
          className="absolute right-0 top-full z-50 mt-2 flex max-h-[min(32rem,80vh)] w-[min(24rem,calc(100vw-2rem))] flex-col overflow-hidden rounded-lg border border-ink/10 bg-paper shadow-xl"
        >
          <div className="flex items-center justify-between gap-2 border-b border-ink/10 px-4 py-2.5">
            <p className="text-sm font-semibold text-ink">Notifications</p>
            {unread > 0 ? (
              <Button variant="ghost" onClick={() => void markRead(null)} className="text-xs">
                <CheckCheck className="mr-1 h-3.5 w-3.5" />
                Mark all read
              </Button>
            ) : null}
          </div>
          <ul className="flex-1 overflow-y-auto">
            {failed && entries.length === 0 ? (
              <li className="px-4 py-6 text-center text-sm text-ink/60">
                Notifications could not be loaded. They will load again shortly.
              </li>
            ) : entries.length === 0 ? (
              <li className="px-4 py-6 text-center text-sm text-ink/60">Nothing here yet.</li>
            ) : (
              entries.map((entry) => (
                <li key={entry.id} className="border-b border-ink/5 last:border-b-0">
                  <button
                    type="button"
                    className={cn(
                      "flex w-full flex-col gap-0.5 px-4 py-3 text-left transition-colors hover:bg-ink/5",
                      entry.read_at === null ? "bg-teal/5" : "",
                    )}
                    onClick={() => {
                      setOpen(false);
                      if (entry.read_at === null) void markRead(entry.id);
                      follow(entry);
                    }}
                  >
                    <span className="flex items-center gap-2">
                      {entry.read_at === null ? (
                        <>
                          <span
                            className="h-2 w-2 shrink-0 rounded-full bg-teal"
                            aria-hidden="true"
                          />
                          <span className="sr-only">Unread:</span>
                        </>
                      ) : null}
                      <span
                        className={cn(
                          "text-sm text-ink",
                          entry.read_at === null ? "font-semibold" : "",
                          entry.severity === "critical" ? "text-red-700 dark:text-red-400" : "",
                        )}
                      >
                        {entry.title}
                      </span>
                    </span>
                    <span className="line-clamp-3 text-sm text-ink/70">{entry.text}</span>
                    <span className="text-xs text-ink/45">{arrived(entry.created_at)}</span>
                  </button>
                </li>
              ))
            )}
          </ul>
          <div className="border-t border-ink/10 px-4 py-2.5 text-xs text-ink/60">
            {alerts === "on" ? (
              <button
                type="button"
                className="inline-flex items-center gap-1.5 hover:text-ink"
                onClick={turnAlertsOff}
              >
                <BellOff className="h-3.5 w-3.5" />
                Turn off {where} on this device
              </button>
            ) : alerts === "off" ? (
              <button
                type="button"
                className="inline-flex items-center gap-1.5 hover:text-ink"
                onClick={() => void turnAlertsOn()}
              >
                <BellRing className="h-3.5 w-3.5" />
                Show {where} on this device
              </button>
            ) : alerts === "blocked" ? (
              <span>
                This browser blocks notifications from this site. Allow them in its site settings.
              </span>
            ) : (
              <span>This browser cannot show notifications.</span>
            )}
          </div>
        </div>
      ) : null}
    </div>
  );
}
