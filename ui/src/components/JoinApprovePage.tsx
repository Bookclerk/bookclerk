import { useEffect, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  answerBranchConsent,
  beginClerkStepUp,
  fetchBranchConsent,
  isConsentRedirect,
  joinErrorMessage,
  type BranchConsentView,
  type ClerkStepUp,
  type ConnectionType,
} from "@/lib/join";
import { assertPasskey, passkeysSupported } from "@/lib/webauthn";

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
 *
 * An Owner of the library may instead let an empty server join **as a
 * clerk** (it shares this library's database and keys). The daemon decides
 * whether to offer it; choosing it needs a fresh authenticator code or
 * passkey, and the code it answers can only be redeemed as a clerk.
 */
export function JoinApprovePage({ onSignOut }: JoinApprovePageProps) {
  const [search] = useState(() => window.location.search);
  const [view, setView] = useState<BranchConsentView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [connection, setConnection] = useState<ConnectionType>("branch");
  const [totpCode, setTotpCode] = useState("");

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

  /** The second factor for a clerk approval, or `null` to stop. */
  async function clerkStepUp(method: "totp" | "passkey"): Promise<ClerkStepUp | null> {
    if (method === "totp") {
      const code = totpCode.replace(/\s+/g, "");
      if (!/^\d{6}$/.test(code)) {
        setError("Enter the 6-digit code from your authenticator app.");
        return null;
      }
      return { method: "totp", code };
    }
    const assertion = await assertPasskey(await beginClerkStepUp());
    return { method: "passkey", ...assertion };
  }

  async function answer(approve: boolean, method?: "totp" | "passkey") {
    setBusy(true);
    setError(null);
    try {
      let clerk: { step_up: ClerkStepUp } | undefined;
      if (approve && connection === "clerk" && method) {
        const stepUp = await clerkStepUp(method);
        if (!stepUp) {
          setBusy(false);
          return;
        }
        clerk = { step_up: stepUp };
      }
      const { redirect } = await answerBranchConsent(search, approve, clerk);
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
  const choice = view.connection?.choice ? view.connection : null;
  const asClerk = choice != null && choice.clerk_available && connection === "clerk";
  const canTotp = asClerk && choice.step_up.includes("totp");
  const canPasskey = asClerk && choice.step_up.includes("passkey") && passkeysSupported();

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
      {choice ? (
        <fieldset className="space-y-2 text-sm text-ink" disabled={busy}>
          <legend className="font-medium">How should it join?</legend>
          <label className="flex items-start gap-2">
            <input
              type="radio"
              name="connection"
              className="mt-0.5"
              checked={connection === "branch"}
              onChange={() => setConnection("branch")}
            />
            <span>Join as a branch</span>
          </label>
          <label className="flex items-start gap-2">
            <input
              type="radio"
              name="connection"
              className="mt-0.5"
              checked={connection === "clerk"}
              disabled={!choice.clerk_available}
              onChange={() => setConnection("clerk")}
            />
            <span>
              <span className="block">Join as a clerk of this library</span>
              {!choice.clerk_available && choice.clerk_reason ? (
                <span className="block text-xs text-ink/55">{choice.clerk_reason}</span>
              ) : null}
            </span>
          </label>
        </fieldset>
      ) : null}
      {asClerk ? (
        <div className="space-y-2 text-sm text-ink/75">
          <p>
            It joins as a <strong>clerk</strong>. That means:
          </p>
          <ul className="list-disc space-y-1 pl-5">
            <li>
              It receives this library's database settings and keys, and serves the same library as
              this server, for every user.
            </li>
            <li>Whatever it held before is replaced.</li>
            <li>It shows under Settings → Clerks here, where it can be removed once stopped.</li>
          </ul>
          <p className="font-medium text-ink">Confirm it's you to continue.</p>
          {canTotp ? (
            <div className="space-y-1">
              <label className="block text-sm font-medium text-ink" htmlFor="clerk-totp">
                Authenticator code
              </label>
              <Input
                id="clerk-totp"
                inputMode="numeric"
                autoComplete="one-time-code"
                maxLength={8}
                value={totpCode}
                onChange={(e) => setTotpCode(e.target.value)}
                disabled={busy}
              />
            </div>
          ) : null}
        </div>
      ) : (
        <div className="space-y-2 text-sm text-ink/75">
          <p>
            It joins as a <strong>branch</strong> you own. That means:
          </p>
          <ul className="list-disc space-y-1 pl-5">
            <li>
              This library can send that branch work for your accounts, such as scans and downloads.
            </li>
            <li>
              The branch's operator approves each of this library's plugins before it runs there,
              and can leave at any time.
            </li>
            <li>The branch keeps its own database; this library's database and keys stay here.</li>
          </ul>
        </div>
      )}
      <div className="space-y-2 text-sm text-ink/75">
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
        {asClerk ? (
          <>
            {canTotp ? (
              <Button
                type="button"
                disabled={busy || !view.enrollment || !totpCode.trim()}
                onClick={() => void answer(true, "totp")}
              >
                Allow as a clerk
              </Button>
            ) : null}
            {canPasskey ? (
              <Button
                type="button"
                variant={canTotp ? "secondary" : "primary"}
                disabled={busy || !view.enrollment}
                onClick={() => void answer(true, "passkey")}
              >
                Allow as a clerk with a passkey
              </Button>
            ) : null}
          </>
        ) : (
          <Button
            type="button"
            disabled={busy || !view.enrollment}
            onClick={() => void answer(true)}
          >
            Allow
          </Button>
        )}
        <Button
          type="button"
          variant="secondary"
          disabled={busy}
          onClick={() => void answer(false)}
        >
          Deny
        </Button>
        <Button type="button" variant="ghost" disabled={busy} onClick={() => void onSignOut()}>
          Not you? Sign out
        </Button>
      </div>
    </JoinShell>
  );
}
