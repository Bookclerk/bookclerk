import { useCallback, useEffect, useRef, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import {
  clerkCallbackRetryable,
  clerkErrorMessage,
  finishClerkSignIn,
  parseClerkCallback,
  type ClerkCallback,
} from "@/lib/clerk";

type Phase =
  | { kind: "working" }
  /** `leftBranch`: what became of the branch the card replaced, if any. */
  | { kind: "signed_in"; library: string; leftBranch: string | null }
  /** `retry`: the request got no answer, so the same code may be sent again. */
  | { kind: "failed"; message: string; retry: boolean };

/** Props for {@link ClerkCallbackPage}. */
export interface ClerkCallbackPageProps {
  /** Opens Settings → Library (where the library cards are listed). */
  onOpenLibrary: () => void;
}

/**
 * `/clerk/callback` on this install: relays the library's `code` and
 * `state` to this install's daemon, which redeems them for a library card
 * with the verifier only it holds, and shows "Signed in to <library>".
 *
 * The query is removed from the address bar before the request, so the
 * code is not kept in history. It stays in memory only so a request that
 * got no answer can offer Try again; any answer ends the daemon's pending
 * sign-in, so a refusal starts over from Settings.
 */
export function ClerkCallbackPage({ onOpenLibrary }: ClerkCallbackPageProps) {
  const [phase, setPhase] = useState<Phase>({ kind: "working" });
  // One redeem per page load (StrictMode runs effects twice).
  const sent = useRef(false);
  // The callback, kept for Try again.
  const pending = useRef<ClerkCallback | null>(null);

  const redeem = useCallback((callback: ClerkCallback) => {
    setPhase({ kind: "working" });
    void finishClerkSignIn(callback)
      .then(({ card, left_branch }) =>
        setPhase({
          kind: "signed_in",
          library: card.library_name || card.origin,
          leftBranch: left_branch
            ? left_branch.owner_notified
              ? "This server is no longer a branch of it: it stopped running the library's work, and the library was told."
              : "This server is no longer a branch of it, but the library could not be told; remove this branch there."
            : null,
        }),
      )
      .catch((err: unknown) =>
        setPhase({
          kind: "failed",
          message: clerkErrorMessage(err, "Could not sign in to the library."),
          retry: clerkCallbackRetryable(err),
        }),
      );
  }, []);

  useEffect(() => {
    if (sent.current) return;
    sent.current = true;
    const callback = parseClerkCallback(window.location.search);
    window.history.replaceState(null, "", window.location.pathname);
    if (!callback) {
      setPhase({
        kind: "failed",
        message:
          "This page has nothing to finish. Start Sign in with a library card in Settings → Library again.",
        retry: false,
      });
      return;
    }
    pending.current = callback;
    redeem(callback);
  }, [redeem]);

  if (phase.kind === "working") {
    return (
      <JoinShell title="Signing in…" lead="Finishing the sign-in with the library.">
        <p className="text-sm text-ink/50">One moment.</p>
      </JoinShell>
    );
  }

  if (phase.kind === "failed") {
    return (
      <JoinShell title="Could not sign in" lead={phase.message}>
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

  return (
    <JoinShell
      title={`Signed in to ${phase.library}`}
      lead={
        <>
          This server now holds a library card for <strong>{phase.library}</strong>. Its books show
          in Library within a minute, yours marked, beside the books here. It downloads nothing from
          that library.
        </>
      }
    >
      {phase.leftBranch ? <p className="text-sm text-ink/75">{phase.leftBranch}</p> : null}
      <div className="flex gap-2">
        <Button type="button" onClick={() => window.location.assign("/library")}>
          Open your books
        </Button>
        <Button type="button" variant="secondary" onClick={onOpenLibrary}>
          Settings → Library
        </Button>
      </div>
    </JoinShell>
  );
}
