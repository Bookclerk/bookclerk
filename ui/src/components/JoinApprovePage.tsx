import { useEffect, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import {
  answerBranchConsent,
  fetchBranchConsent,
  isConsentRedirect,
  joinErrorMessage,
  type BranchConsentView,
} from "@/lib/join";

/** Props for {@link JoinApprovePage}. */
export interface JoinApprovePageProps {
  /** Signs out (to sign in with another account). */
  onSignOut: () => void | Promise<void>;
}

/**
 * `/join/approve` on the library: "Let <branch> join <library> as yours?"
 *
 * `/oidc/authorize` sends a branch's sign-in here once its request is well
 * formed. The signed-in user allows or denies; the daemon then answers where
 * to send the browser (the branch's callback with a single-use code, or
 * `error=access_denied`), and this page goes there only when it is the
 * request's own redirect URI.
 */
export function JoinApprovePage({ onSignOut }: JoinApprovePageProps) {
  const [search] = useState(() => window.location.search);
  const [view, setView] = useState<BranchConsentView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchBranchConsent(search)
      .then((body) => {
        if (!cancelled) setView(body);
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(
            joinErrorMessage(
              err,
              "This request to join is not valid. Start again from the joining server.",
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
      const { redirect } = await answerBranchConsent(search, approve);
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

  if (!view) {
    return (
      <JoinShell title="Join a library" lead={error ?? "Loading the request…"}>
        {error ? (
          <Button type="button" variant="ghost" onClick={() => void onSignOut()}>
            Sign in with another account
          </Button>
        ) : null}
      </JoinShell>
    );
  }

  const who = view.user.display_name?.trim() || `user #${view.user.id}`;

  return (
    <JoinShell
      title={`Let ${view.branch_name} join ${view.library_name} as yours?`}
      lead={
        <>
          Signed in as <strong>{who}</strong>. The server asking is at{" "}
          <span className="font-mono text-xs">{view.redirect_origin}</span>.
        </>
      }
    >
      <div className="space-y-2 text-sm text-ink/75">
        <p>
          It joins as a <strong>branch</strong> you own. That means:
        </p>
        <ul className="list-disc space-y-1 pl-5">
          <li>This library can send that branch work for your accounts, such as scans and downloads.</li>
          <li>
            The branch's operator approves each of this library's plugins before it runs there, and
            can leave at any time.
          </li>
          <li>The branch keeps its own database; this library's database and keys stay here.</li>
        </ul>
        {!view.enrollment ? (
          <p className="font-medium text-brick">This library isn't accepting new branches.</p>
        ) : null}
      </div>
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      <div className="flex flex-wrap gap-2">
        <Button
          type="button"
          disabled={busy || !view.enrollment}
          onClick={() => void answer(true)}
        >
          Allow
        </Button>
        <Button type="button" variant="secondary" disabled={busy} onClick={() => void answer(false)}>
          Deny
        </Button>
        <Button type="button" variant="ghost" disabled={busy} onClick={() => void onSignOut()}>
          Not you? Sign out
        </Button>
      </div>
    </JoinShell>
  );
}
