import { useEffect, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  answerBranchConsent,
  beginSharedBranchStepUp,
  fetchBranchConsent,
  isConsentRedirect,
  joinErrorMessage,
  type BranchConsentView,
  type SharedBranchStepUp,
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
 * shared branch** (it shares this library's database and keys). The daemon decides
 * whether to offer it; choosing it needs a fresh authenticator code or
 * passkey, and the code it answers can only be redeemed as a shared branch.
 */
export function JoinApprovePage({ onSignOut }: JoinApprovePageProps) {
  const [search] = useState(() => window.location.search);
  const [view, setView] = useState<BranchConsentView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [connection, setConnection] = useState<ConnectionType>("branch");
  const [totpCode, setTotpCode] = useState("");
  const [bringConnections, setBringConnections] = useState(false);

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

  /** The second factor for a shared branch approval, or `null` to stop. */
  async function sharedBranchStepUp(
    method: "totp" | "passkey",
  ): Promise<SharedBranchStepUp | null> {
    if (method === "totp") {
      const code = totpCode.replace(/\s+/g, "");
      if (!/^\d{6}$/.test(code)) {
        setError("Enter the 6-digit code from your authenticator app.");
        return null;
      }
      return { method: "totp", code };
    }
    const assertion = await assertPasskey(await beginSharedBranchStepUp());
    return { method: "passkey", ...assertion };
  }

  async function answer(approve: boolean, method?: "totp" | "passkey") {
    setBusy(true);
    setError(null);
    try {
      let shared_branch: { step_up: SharedBranchStepUp } | undefined;
      if (approve && connection === "shared_branch" && method) {
        const stepUp = await sharedBranchStepUp(method);
        if (!stepUp) {
          setBusy(false);
          return;
        }
        shared_branch = { step_up: stepUp };
      }
      const { redirect } = await answerBranchConsent(
        search,
        approve,
        shared_branch,
        bringConnections,
      );
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
  const asSharedBranch =
    choice != null && choice.shared_branch_available && connection === "shared_branch";
  const canTotp = asSharedBranch && choice.step_up.includes("totp");
  const canPasskey = asSharedBranch && choice.step_up.includes("passkey") && passkeysSupported();
  const connections = view.connections ?? [];
  const travelling = connections.filter((c) => c.travels);
  const staying = connections.filter((c) => !c.travels);

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
            <span>Join as a branch with its own database</span>
          </label>
          <label className="flex items-start gap-2">
            <input
              type="radio"
              name="connection"
              className="mt-0.5"
              checked={connection === "shared_branch"}
              disabled={!choice.shared_branch_available}
              onChange={() => setConnection("shared_branch")}
            />
            <span>
              <span className="block">Join as a shared branch of this library</span>
              {!choice.shared_branch_available && choice.shared_branch_reason ? (
                <span className="block text-xs text-ink/55">{choice.shared_branch_reason}</span>
              ) : null}
            </span>
          </label>
        </fieldset>
      ) : null}
      {asSharedBranch ? (
        <div className="space-y-2 text-sm text-ink/75">
          <p>
            It joins as a <strong>shared branch</strong>, on this library's database. That means:
          </p>
          <ul className="list-disc space-y-1 pl-5">
            <li>
              It receives this library's database settings and keys, and serves the same library as
              this server, for every user.
            </li>
            <li>Whatever it held before is replaced.</li>
            <li>
              It shows under Settings → Shared branches here, where it can be removed once stopped.
            </li>
          </ul>
          <p className="font-medium text-ink">Confirm it's you to continue.</p>
          {canTotp ? (
            <div className="space-y-1">
              <label className="block text-sm font-medium text-ink" htmlFor="shared-branch-totp">
                Authenticator code
              </label>
              <Input
                id="shared-branch-totp"
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
          {connections.length > 0 ? (
            <div className="space-y-1 pt-1">
              {travelling.length > 0 ? (
                <>
                  <label className="flex items-start gap-2 text-ink">
                    <input
                      type="checkbox"
                      className="mt-0.5"
                      checked={bringConnections}
                      disabled={busy}
                      onChange={(e) => setBringConnections(e.target.checked)}
                    />
                    <span>Bring my store connections to this branch</span>
                  </label>
                  <ul className="list-disc space-y-0.5 pl-11 text-xs text-ink/65">
                    {travelling.map((c) => (
                      <li key={`${c.source}:${c.account_id}`}>
                        {c.source}: {c.label?.trim() || c.account_id}
                      </li>
                    ))}
                  </ul>
                </>
              ) : null}
              {staying.length > 0 ? (
                <p className={`text-xs text-ink/55 ${travelling.length > 0 ? "pl-6" : ""}`}>
                  Stays here: {staying.map((c) => c.source).join(", ")}. Connect{" "}
                  {staying.length === 1 ? "it" : "them"} on the branch yourself if you want{" "}
                  {staying.length === 1 ? "it" : "them"} there.
                </p>
              ) : null}
              {travelling.length > 0 ? (
                <p className="pl-6 text-xs text-ink/55">
                  The branch's administrator can turn off store connections for you; then nothing is
                  added there.
                </p>
              ) : null}
            </div>
          ) : null}
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
        {asSharedBranch ? (
          <>
            {canTotp ? (
              <Button
                type="button"
                disabled={busy || !view.enrollment || !totpCode.trim()}
                onClick={() => void answer(true, "totp")}
              >
                Allow as a shared branch
              </Button>
            ) : null}
            {canPasskey ? (
              <Button
                type="button"
                variant={canTotp ? "secondary" : "primary"}
                disabled={busy || !view.enrollment}
                onClick={() => void answer(true, "passkey")}
              >
                Allow as a shared branch with a passkey
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
