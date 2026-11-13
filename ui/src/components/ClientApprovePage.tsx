import { useEffect, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import {
  answerClientConsent,
  fetchClientConsent,
  isConsentRedirect,
  joinErrorMessage,
  type ClientConsentView,
} from "@/lib/join";

/** Props for {@link ClientApprovePage}. */
export interface ClientApprovePageProps {
  /** Signs out (to sign in with another account). */
  onSignOut: () => void | Promise<void>;
}

/**
 * `/client/approve` on the library: "Give <client> a library card for <library>?"
 *
 * `/oidc/authorize` sends a client app's sign-in here once its request is
 * well formed. A client app shows a person's books from this library without
 * joining it as a branch. The signed-in user allows or denies; the daemon
 * then answers where to send the browser (the client app's callback with a
 * single-use code, or `error=access_denied`), and this page goes there only
 * when it is the request's own redirect URI.
 */
export function ClientApprovePage({ onSignOut }: ClientApprovePageProps) {
  const [search] = useState(() => window.location.search);
  const [view, setView] = useState<ClientConsentView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchClientConsent(search)
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
      const { redirect } = await answerClientConsent(search, approve);
      if (!isConsentRedirect(redirect, search)) {
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
      <JoinShell title="Library card" lead={error ?? "Loading the request…"}>
        {error ? (
          <Button type="button" variant="ghost" onClick={() => void signOut()}>
            Sign in with another account
          </Button>
        ) : null}
      </JoinShell>
    );
  }

  const who = view.user.display_name?.trim() || `user #${view.user.id}`;

  return (
    <JoinShell
      title={`Give ${view.client_name} a library card for ${view.library_name}?`}
      lead={
        <>
          Signed in as <strong>{who}</strong>. The app asking is at{" "}
          <span className="font-mono text-xs">{view.redirect_origin}</span>.
        </>
      }
    >
      <div className="space-y-2 text-sm text-ink/75">
        <p>With a library card, that app can, as you:</p>
        <ul className="list-disc space-y-1 pl-5">
          <li>Show your books, Discover and your wishlist from this library.</li>
          <li>Change your own preferences and profile here.</li>
        </ul>
        <p>It can't change this library's settings, and this library sends it no work.</p>
        <p>
          You can take the card back at any time: sign it out from your sessions in Settings. It
          also stops working after 90 days unused.
        </p>
      </div>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
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
