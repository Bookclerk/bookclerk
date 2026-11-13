import { useCallback, useEffect, useRef, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { isApiError } from "@/lib/api";
import { isDesktopWindow } from "@/lib/desktop";
import {
  broughtConnectionsSummary,
  finishJoin,
  isSharedBranchJoined,
  joinErrorMessage,
  makeWindowOf,
  parseJoinCallback,
  type JoinCallback,
} from "@/lib/join";

type Phase =
  | { kind: "working" }
  /**
   * `connections`: what happened to the store connections the person brought, if any.
   * `window`: in the desktop window, whether downloading was turned off
   * (`null` outside the window, where a join makes an ordinary branch).
   */
  | {
      kind: "connected";
      library: string;
      running: boolean;
      connections: string | null;
      window: boolean | null;
    }
  | { kind: "shared_branch"; library: string; wrapped: boolean }
  /** `retry`: the daemon kept the sign-in, so the same code can be sent again. */
  | { kind: "failed"; message: string; retry: boolean };

/** Props for {@link JoinCallbackPage}. */
export interface JoinCallbackPageProps {
  /** Opens Settings → Library (the consent list). */
  onOpenLibrary: () => void;
  /** Whether the app runs in the desktop window (defaults to detecting it). */
  desktop?: boolean;
}

/**
 * `/join/callback` on the joining install: relays the library's `code`
 * and `state` to this install's daemon, which redeems them with the
 * verifier only it holds, and shows "Connected to <library>".
 *
 * The query is removed from the address bar before the request, so the
 * code is not kept in history; it stays in memory only so a failure the
 * daemon marks retryable (`join_retry`: the library unreachable; or a
 * shared branch's missing database plugin or D1 token, once fixed), or a request
 * that got no answer at all, can offer Try again.
 *
 * For a shared branch join (`connection=shared_branch`) the daemon receives the library's
 * database settings and keys sealed to this node, writes them as its own,
 * and restarts; this page only says so (no key ever reaches the browser).
 *
 * In the desktop window a branch join makes this computer a **window** of
 * the library: downloading is turned off before "Connected" shows, so it
 * shows the person's books there and runs no work until they turn
 * downloading on in Settings → Library.
 */
export function JoinCallbackPage({
  onOpenLibrary,
  desktop = isDesktopWindow(),
}: JoinCallbackPageProps) {
  const [phase, setPhase] = useState<Phase>({ kind: "working" });
  // One redeem per page load (StrictMode runs effects twice).
  const sent = useRef(false);

  // The callback, kept for Try again.
  const pending = useRef<JoinCallback | null>(null);

  const redeem = useCallback(
    (callback: JoinCallback) => {
      setPhase({ kind: "working" });
      void finishJoin(callback)
        .then(async (joined) => {
          if (isSharedBranchJoined(joined)) {
            setPhase({
              kind: "shared_branch",
              library: joined.library_name || joined.cluster_id,
              wrapped: joined.master_key_wrapped,
            });
            return;
          }
          const asWindow = desktop ? await makeWindowOf(joined.cluster) : null;
          setPhase({
            kind: "connected",
            library: joined.library_name || joined.cluster.owner_origin,
            running: joined.running,
            connections: broughtConnectionsSummary(joined.connections),
            window: asWindow,
          });
        })
        .catch((err: unknown) =>
          setPhase({
            kind: "failed",
            message: joinErrorMessage(err, "Could not join the library."),
            // A request that never got an answer may be tried again: the
            // pending join stays until the library's answer is recorded. So
            // may a shared branch join this server could not use yet (a missing
            // database plugin or D1 token): the daemon keeps the sign-in.
            retry:
              !isApiError(err) ||
              err.code === "join_retry" ||
              err.code === "database_guest_missing" ||
              err.code === "d1_token_missing",
          }),
        );
    },
    [desktop],
  );

  useEffect(() => {
    if (sent.current) return;
    sent.current = true;
    const callback = parseJoinCallback(window.location.search);
    window.history.replaceState(null, "", window.location.pathname);
    if (!callback) {
      setPhase({
        kind: "failed",
        message: "This page has nothing to finish. Start Join a library again.",
        retry: false,
      });
      return;
    }
    pending.current = callback;
    redeem(callback);
  }, [redeem]);

  if (phase.kind === "working") {
    return (
      <JoinShell title="Joining…" lead="Finishing the sign-in with the library.">
        <p className="text-sm text-ink/50">One moment.</p>
      </JoinShell>
    );
  }

  if (phase.kind === "failed") {
    return (
      <JoinShell title="Could not join" lead={phase.message}>
        <div className="flex gap-2">
          {phase.retry && pending.current ? (
            <Button
              type="button"
              onClick={() => {
                if (pending.current) redeem(pending.current);
              }}
            >
              Try again
            </Button>
          ) : null}
          <Button
            type="button"
            variant={phase.retry ? "secondary" : "primary"}
            onClick={onOpenLibrary}
          >
            Back to Settings → Library
          </Button>
        </div>
      </JoinShell>
    );
  }

  if (phase.kind === "shared_branch") {
    return (
      <JoinShell
        title={`Joined ${phase.library} as a shared branch`}
        lead={
          <>
            This server now shares <strong>{phase.library}</strong>'s database and keys. It is
            restarting to open them; sign in again with your account on the library once it is back.
          </>
        }
      >
        <p className="text-sm text-ink/55">
          {phase.wrapped
            ? "The library's key is stored here wrapped with this server's own password."
            : "The library's key is stored here unwrapped. Set BOOKCLERK_AUTH_PASSWORD on this server to wrap it at rest."}
        </p>
        <Button type="button" onClick={() => window.location.assign("/")}>
          Open the library
        </Button>
      </JoinShell>
    );
  }

  if (phase.window != null) {
    return (
      <JoinShell
        title={`Connected to ${phase.library}`}
        lead={
          phase.window ? (
            <>
              This computer is now a window of <strong>{phase.library}</strong>: it shows your books
              there and downloads nothing. To keep copies here too, turn on "Use this computer to
              download" in Settings → Library.
            </>
          ) : (
            <>
              This computer joined <strong>{phase.library}</strong>, but downloading could not be
              turned off, so it is a branch that downloads. Turn off "Use this computer to download"
              in Settings → Library to make it a window.
            </>
          )
        }
      >
        {phase.connections ? <p className="text-sm text-ink/75">{phase.connections}</p> : null}
        <div className="flex gap-2">
          <Button type="button" onClick={() => window.location.assign("/")}>
            Open your books
          </Button>
          <Button type="button" variant="secondary" onClick={onOpenLibrary}>
            Settings → Library
          </Button>
        </div>
      </JoinShell>
    );
  }

  return (
    <JoinShell
      title={`Connected to ${phase.library}`}
      lead={
        <>
          This server is now a branch of <strong>{phase.library}</strong>
          {phase.running ? " and is connected." : "; it connects within a few seconds."} When the
          library asks to run a plugin here, it waits for your approval.
        </>
      }
    >
      {phase.connections ? <p className="text-sm text-ink/75">{phase.connections}</p> : null}
      <Button type="button" onClick={onOpenLibrary}>
        Review plugin requests
      </Button>
    </JoinShell>
  );
}
