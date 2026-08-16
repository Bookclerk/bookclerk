import { useMemo } from "react";
import { Cable, ChevronRight, Monitor, RefreshCw, Smartphone, Tablet } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { type ListedSession } from "@/lib/api";

function sessionDeviceIcon(deviceType: string | null | undefined) {
  switch ((deviceType ?? "").toLowerCase()) {
    case "mobile":
      return Smartphone;
    case "tablet":
      return Tablet;
    case "api":
      return Cable;
    default:
      return Monitor;
  }
}

function sessionLabel(row: ListedSession): string {
  const label = row.client_label?.trim();
  if (label) return label;
  if ((row.device_type ?? "").toLowerCase() === "api" || row.kind === "operator") {
    return "API";
  }
  return "Unknown";
}

function formatSessionWhen(iso: string | null | undefined): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function SessionMeta({ row }: { row: ListedSession }) {
  return (
    <p className="text-xs text-ink/50">
      Signed in {formatSessionWhen(row.created_at)}
      {row.last_used_at ? ` · Last active ${formatSessionWhen(row.last_used_at)}` : null}
      {" · Expires "}
      {formatSessionWhen(row.expires_at)}
    </p>
  );
}

function SessionBadges({ row }: { row: ListedSession }) {
  return (
    <>
      {row.elevated ? (
        <Badge className="bg-teal/15 text-ink normal-case tracking-normal">Elevated</Badge>
      ) : null}
      {row.impersonating_user_id ? (
        <Badge className="bg-brick/10 text-brick normal-case tracking-normal">
          Impersonating #{row.impersonating_user_id}
        </Badge>
      ) : null}
    </>
  );
}

function CurrentSessionCard({ row }: { row: ListedSession }) {
  const Icon = sessionDeviceIcon(row.device_type);
  return (
    <div className="rounded-md border border-teal/25 bg-teal/5 px-4 py-3">
      <div className="flex items-start gap-3">
        <div className="mt-0.5 flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-card-strong text-ink/70">
          <Icon className="h-5 w-5" aria-hidden />
        </div>
        <div className="min-w-0 flex-1 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <span className="font-semibold text-ink">{sessionLabel(row)}</span>
            <Badge className="bg-teal/20 text-ink normal-case tracking-normal">Active now</Badge>
            <SessionBadges row={row} />
          </div>
          <SessionMeta row={row} />
        </div>
      </div>
    </div>
  );
}

function OtherSessionRow({
  row,
  busy,
  onRevoke,
}: {
  row: ListedSession;
  busy: boolean;
  onRevoke: (id: number) => void;
}) {
  const Icon = sessionDeviceIcon(row.device_type);
  return (
    <li className="flex items-center gap-3 px-3 py-3">
      <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-ink/5 text-ink/60">
        <Icon className="h-4 w-4" aria-hidden />
      </div>
      <div className="min-w-0 flex-1">
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-medium text-ink">{sessionLabel(row)}</span>
          <SessionBadges row={row} />
        </div>
        <SessionMeta row={row} />
      </div>
      <Button
        type="button"
        variant="secondary"
        className="h-8 shrink-0"
        disabled={busy}
        onClick={() => onRevoke(row.id)}
      >
        Revoke
        <ChevronRight className="h-4 w-4 opacity-60" aria-hidden />
      </Button>
    </li>
  );
}

/**
 * Twitter-style session list — current device, bulk logout, and other sessions.
 */
export function SessionsPanel({
  sessions,
  busy,
  error,
  onRefresh,
  onRevoke,
  onRevokeOthers,
  embedded = false,
}: {
  sessions: ListedSession[];
  busy: boolean;
  error: string | null;
  onRefresh: () => void;
  onRevoke: (id: number) => void;
  onRevokeOthers: () => void;
  /** When true, omit the outer Sessions heading (parent already provides one). */
  embedded?: boolean;
}) {
  const { current, others } = useMemo(() => {
    const currentSession = sessions.find((row) => row.is_current) ?? null;
    const otherSessions = sessions.filter((row) => !row.is_current);
    return { current: currentSession, others: otherSessions };
  }, [sessions]);

  return (
    <section className="space-y-4">
      {embedded ? null : (
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-lg font-semibold text-ink">Sessions</h2>
            <p className="text-sm text-ink/55">
              These are the devices and clients currently signed in to your account. Revoke any
              session you do not recognize.
            </p>
          </div>
          <Button type="button" variant="secondary" disabled={busy} onClick={onRefresh}>
            <RefreshCw className="h-4 w-4" />
            Refresh
          </Button>
        </div>
      )}

      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}

      {sessions.length === 0 ? (
        <p className="bg-card px-3 py-3 text-sm text-ink/50">No sessions returned.</p>
      ) : (
        <>
          {current ? (
            <div className="space-y-2">
              <h3 className="text-sm font-semibold text-ink/80">Current active session</h3>
              <CurrentSessionCard row={current} />
            </div>
          ) : null}

          {others.length > 0 ? (
            <div className="space-y-3">
              <div className="space-y-1">
                <h3 className="text-sm font-semibold text-ink/80">Log out of other sessions</h3>
                <p className="text-sm text-ink/55">
                  {others.length === 1
                    ? "There is 1 other session signed in to your account."
                    : `There are ${others.length} other sessions signed in to your account.`}{" "}
                  This will not sign out your current session.
                </p>
              </div>
              <Button
                type="button"
                variant="danger"
                disabled={busy}
                onClick={onRevokeOthers}
              >
                Log out of all other sessions
              </Button>
              <ul className="divide-y divide-ink/10 bg-card">
                {others.map((row) => (
                  <OtherSessionRow
                    key={`${row.kind}-${row.id}`}
                    row={row}
                    busy={busy}
                    onRevoke={onRevoke}
                  />
                ))}
              </ul>
            </div>
          ) : current ? (
            <p className="text-sm text-ink/50">No other active sessions.</p>
          ) : null}
        </>
      )}
    </section>
  );
}
