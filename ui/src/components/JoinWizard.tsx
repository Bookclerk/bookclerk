import { useEffect, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  fetchJoinStatus,
  fingerprintGroups,
  inspectLibrary,
  JoinApiError,
  joinErrorMessage,
  joinWithInvite,
  looksLikeBranchInvite,
  makeWindowOf,
  startJoin,
  type BranchLinkView,
  type LibraryInfoView,
} from "@/lib/join";
import { isDesktopWindow } from "@/lib/desktop";
import { cn } from "@/lib/utils";

type Tab = "signin" | "invite";

/** Props for {@link JoinWizard}. */
export interface JoinWizardProps {
  /** Suggested branch name (this machine's name). */
  defaultBranchName?: string;
  /**
   * Called after an invite joined (the sign-in path leaves the page), with
   * the library's origin and the link the join recorded. `asWindow` is,
   * in the desktop window, whether downloading was turned off for the new
   * link (`null` elsewhere).
   */
  onJoined?: (libraryOrigin: string, link: BranchLinkView, asWindow: boolean | null) => void;
  /** Whether the app runs in the desktop window (defaults to detecting it). */
  desktop?: boolean;
  /** Shows a Back button that calls this. */
  onCancel?: () => void;
}

/**
 * Join a library: address → fingerprint (self-signed libraries) → sign in
 * on the library, which sends the browser back to `/join/callback`. The
 * "I have an invite" tab takes a pasted branch invite instead; in the
 * desktop window an invite join makes a window of the library, as
 * `/join/callback` does for a sign-in.
 *
 * Nothing secret is kept here: the daemon holds the PKCE verifier and the
 * node keys, and an invite goes straight to it in a POST body.
 *
 * An empty install may also offer to join **as a shared branch**: it then shares
 * the library's database and keys, and what it holds now is replaced. The
 * library's Owner makes the final choice on the consent page; the daemon
 * checks again that this install is empty before it applies anything.
 */
export function JoinWizard({
  defaultBranchName,
  onJoined,
  onCancel,
  desktop = isDesktopWindow(),
}: JoinWizardProps) {
  const [tab, setTab] = useState<Tab>("signin");
  const [address, setAddress] = useState("");
  const [branchName, setBranchName] = useState(defaultBranchName ?? "");
  const [library, setLibrary] = useState<LibraryInfoView | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [invite, setInvite] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [suggested, setSuggested] = useState<string | null>(null);
  const [sharedBranchEligible, setSharedBranchEligible] = useState(false);
  const [asSharedBranch, setAsSharedBranch] = useState(false);
  const [replaceConfirmed, setReplaceConfirmed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void fetchJoinStatus()
      .then((status) => {
        if (!cancelled) setSharedBranchEligible(status.shared_branch_eligible === true);
      })
      .catch(() => {
        // Branch only; the daemon refuses a shared branch offer anyway.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  async function onInspect(event: FormEvent) {
    event.preventDefault();
    setError(null);
    setSuggested(null);
    setLibrary(null);
    setConfirmed(false);
    setBusy(true);
    try {
      const { library: info } = await inspectLibrary(address.trim());
      setLibrary(info);
      if (!info.enrollment) {
        setError("This library isn't accepting new branches.");
      }
    } catch (err) {
      // The library answers to another address: offer it.
      setSuggested(err instanceof JoinApiError && err.origin ? err.origin : null);
      setError(joinErrorMessage(err, "Could not reach that library."));
    } finally {
      setBusy(false);
    }
  }

  async function onSignIn() {
    if (!library) return;
    setError(null);
    setBusy(true);
    try {
      const started = await startJoin({
        address: library.origin,
        fingerprint: library.needs_fingerprint ? library.fingerprint : null,
        branch_name: branchName,
        shared_branch: sharedBranchEligible && asSharedBranch,
      });
      // The library signs the user in and sends the browser back to
      // this install's /join/callback.
      window.location.assign(started.authorize_url);
    } catch (err) {
      setError(joinErrorMessage(err, "Could not start the sign-in."));
      setBusy(false);
    }
  }

  async function onInvite(event: FormEvent) {
    event.preventDefault();
    setError(null);
    if (!looksLikeBranchInvite(invite)) {
      setError("That is not a branch invite. It looks like https://<library>/enroll#bcn1…");
      return;
    }
    setBusy(true);
    try {
      const joined = await joinWithInvite(invite);
      // In the desktop window a join makes a window of the library. The
      // form stays busy until that is stored.
      const asWindow = desktop ? await makeWindowOf(joined.cluster) : null;
      setInvite("");
      onJoined?.(joined.cluster.owner_origin, joined.cluster, asWindow);
    } catch (err) {
      setError(joinErrorMessage(err, "Could not join with that invite."));
    } finally {
      setBusy(false);
    }
  }

  const canSignIn =
    library != null &&
    library.enrollment &&
    (!library.needs_fingerprint || confirmed) &&
    (!asSharedBranch || replaceConfirmed);

  return (
    <div className="space-y-5">
      <div
        className="flex gap-1 rounded-md border border-ink/10 bg-paper p-1"
        role="tablist"
        aria-label="How to join"
      >
        {(
          [
            ["signin", "Sign in"],
            ["invite", "I have an invite"],
          ] as const
        ).map(([id, label]) => (
          <button
            key={id}
            type="button"
            role="tab"
            aria-selected={tab === id}
            className={cn(
              "flex-1 rounded px-3 py-1.5 text-sm font-medium transition-colors",
              tab === id ? "bg-ink text-paper shadow-sm" : "text-ink/60 hover:text-ink",
            )}
            onClick={() => {
              setTab(id);
              setError(null);
            }}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === "signin" ? (
        <div className="space-y-5">
          <form className="space-y-2" onSubmit={(e) => void onInspect(e)}>
            <label className="block text-sm font-medium text-ink" htmlFor="join-address">
              Library address
            </label>
            <div className="flex gap-2">
              <Input
                id="join-address"
                placeholder="https://library.example"
                autoComplete="url"
                value={address}
                onChange={(e) => {
                  setAddress(e.target.value);
                  setLibrary(null);
                }}
                disabled={busy}
              />
              <Button type="submit" disabled={busy || !address.trim()}>
                Continue
              </Button>
            </div>
            <p className="text-xs text-ink/50">
              The address you use to open that library in a browser. This server reads it, not your
              browser.
            </p>
            {suggested ? (
              <Button type="button" variant="ghost" onClick={() => setAddress(suggested)}>
                Use {suggested}
              </Button>
            ) : null}
          </form>

          {library && library.enrollment ? (
            <div className="space-y-4 rounded-md border border-ink/10 bg-paper p-4">
              <p className="text-sm text-ink">
                Found <strong>{library.library_name}</strong>{" "}
                <span className="text-ink/50">({library.origin})</span>
              </p>
              {library.needs_fingerprint && library.fingerprint ? (
                <div className="space-y-2">
                  <p className="text-sm text-ink/70">
                    This library uses its own certificate. Check that this fingerprint matches the
                    one on the library's <strong>Settings → Branches</strong> page:
                  </p>
                  <p className="break-all rounded bg-card px-3 py-2 font-mono text-xs text-ink">
                    {fingerprintGroups(library.fingerprint).join(" ")}
                  </p>
                  <label className="flex items-center gap-2 text-sm text-ink">
                    <input
                      type="checkbox"
                      checked={confirmed}
                      onChange={(e) => setConfirmed(e.target.checked)}
                    />
                    It matches
                  </label>
                </div>
              ) : null}
              <div className="space-y-1">
                <label className="block text-sm font-medium text-ink" htmlFor="join-branch-name">
                  Name this branch
                </label>
                <Input
                  id="join-branch-name"
                  maxLength={64}
                  value={branchName}
                  onChange={(e) => setBranchName(e.target.value)}
                  disabled={busy}
                />
                <p className="text-xs text-ink/50">The library's users see this name.</p>
              </div>
              {sharedBranchEligible ? (
                <div className="space-y-2">
                  <label className="flex items-start gap-2 text-sm text-ink">
                    <input
                      type="checkbox"
                      className="mt-0.5"
                      checked={asSharedBranch}
                      disabled={busy}
                      onChange={(e) => {
                        setAsSharedBranch(e.target.checked);
                        setReplaceConfirmed(false);
                      }}
                    />
                    <span>
                      <span className="block font-medium">Offer to join as a shared branch</span>
                      <span className="block text-ink/55">
                        This server would share the library's database and keys instead of keeping
                        its own. Only an Owner of the library can choose it, and only when the
                        library uses PostgreSQL or Cloudflare D1. Otherwise this server joins as a
                        branch with its own database.
                      </span>
                    </span>
                  </label>
                  {asSharedBranch ? (
                    <label
                      className="flex items-start gap-2 rounded border border-brick/30 bg-brick/5 p-3 text-sm text-ink"
                      role="alert"
                    >
                      <input
                        type="checkbox"
                        className="mt-0.5"
                        checked={replaceConfirmed}
                        disabled={busy}
                        onChange={(e) => setReplaceConfirmed(e.target.checked)}
                      />
                      <span>
                        As a shared branch, this server's own database settings and keys are
                        replaced by the library's, and it restarts. I understand that whatever it
                        holds now is replaced.
                      </span>
                    </label>
                  ) : null}
                </div>
              ) : null}
              <Button type="button" disabled={busy || !canSignIn} onClick={() => void onSignIn()}>
                Sign in to {library.library_name}
              </Button>
            </div>
          ) : null}
        </div>
      ) : (
        <form className="space-y-2" onSubmit={(e) => void onInvite(e)}>
          <label className="block text-sm font-medium text-ink" htmlFor="join-invite">
            Branch invite
          </label>
          <Input
            id="join-invite"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder="https://library.example/enroll#bcn1…"
            value={invite}
            onChange={(e) => setInvite(e.target.value)}
            disabled={busy}
          />
          <p className="text-xs text-ink/50">
            Someone on the library made this with <strong>Add a branch</strong>. It works once, and
            this page does not keep it.
          </p>
          <Button type="submit" disabled={busy || !invite.trim()}>
            Join
          </Button>
        </form>
      )}

      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}

      {onCancel ? (
        <Button type="button" variant="ghost" onClick={onCancel} disabled={busy}>
          Back
        </Button>
      ) : null}
    </div>
  );
}
