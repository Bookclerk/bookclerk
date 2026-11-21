import { useEffect, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { joinErrorMessage } from "@/lib/join";
import {
  answerPlayerConsent,
  fetchPlayerConsent,
  isPlayerReturn,
  type PlayerConsentView,
} from "@/lib/playerApps";

/** Props for {@link PlayerApprovePage}. */
export interface PlayerApprovePageProps {
  /** Signs out (to sign in with another account). */
  onSignOut: () => void | Promise<void>;
}

/**
 * `/apps/approve`: "Let <app> play your audiobooks?"
 *
 * A player app's browser sign-in (`/player/auth/openid`) opens this page.
 * The signed-in patron allows or declines; the daemon answers the
 * library's own way back to the app, and this page goes there only when
 * it is that path on this origin.
 */
export function PlayerApprovePage({ onSignOut }: PlayerApprovePageProps) {
  const [search] = useState(() => window.location.search);
  const [view, setView] = useState<PlayerConsentView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchPlayerConsent(search)
      .then((body) => {
        if (!cancelled) setView(body);
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(
            joinErrorMessage(
              err,
              "This sign-in request is not valid. Start again from the app that asked.",
            ),
          );
        }
      });
    return () => {
      cancelled = true;
    };
  }, [search]);

  async function answer(approve: boolean) {
    setBusy(true);
    setError(null);
    try {
      const { redirect } = await answerPlayerConsent(search, approve);
      if (!isPlayerReturn(redirect)) {
        setError("The answer pointed somewhere unexpected; nothing was sent.");
        setBusy(false);
        return;
      }
      window.location.assign(redirect);
    } catch (err) {
      setError(joinErrorMessage(err, "Could not answer the request."));
      setBusy(false);
    }
  }

  /** Signs out with every control disabled, so Allow can't race it. */
  async function signOut() {
    setBusy(true);
    setError(null);
    try {
      await onSignOut();
    } catch (err) {
      setError(joinErrorMessage(err, "Could not sign out."));
    } finally {
      setBusy(false);
    }
  }

  if (!view) {
    return (
      <JoinShell title="Player app" lead={error ?? "Loading the request…"}>
        {error ? (
          <Button type="button" variant="ghost" onClick={() => void signOut()}>
            Sign in with another account
          </Button>
        ) : null}
      </JoinShell>
    );
  }

  const who = view.display_name?.trim() || "you";

  return (
    <JoinShell
      title={`Let ${view.app_name} play your audiobooks?`}
      lead={
        <>
          Signed in as <strong>{who}</strong>.
        </>
      }
    >
      <div className="space-y-2 text-sm text-ink/75">
        <p>As you, the app can:</p>
        <ul className="list-disc space-y-1 pl-5">
          <li>List and play the audiobooks you can see in this library.</li>
          <li>Keep your place, finished books and bookmarks in step with this library.</li>
          <li>Save books for offline listening, if downloads are on for you.</li>
        </ul>
        <p>It can't change this library's settings or anyone else's listening.</p>
        <p>
          You can sign the app out at any time from your sessions in Settings. It stops working
          after 30 days unused.
        </p>
      </div>
      {error ? (
        <p className="text-sm font-medium text-brick-text" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        <Button type="button" disabled={busy} onClick={() => void answer(true)}>
          Allow
        </Button>
        <Button
          type="button"
          variant="secondary"
          disabled={busy}
          onClick={() => void answer(false)}
        >
          Deny
        </Button>
        <Button type="button" variant="ghost" disabled={busy} onClick={() => void signOut()}>
          Not you? Sign out
        </Button>
      </div>
    </JoinShell>
  );
}
