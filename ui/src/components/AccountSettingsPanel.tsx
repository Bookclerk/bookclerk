import { useEffect, useState, type FormEvent } from "react";
import { Fingerprint, KeyRound, Trash2 } from "lucide-react";
import { SessionsPanel } from "@/components/SessionsPanel";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  deletePasskey,
  deleteUser,
  elevate,
  endElevate,
  isApiError,
  listOidcIdentities,
  listOidcProviders,
  listPasskeys,
  passkeyElevateBegin,
  passkeyElevateFinish,
  passkeyRegisterBegin,
  passkeyRegisterFinish,
  setPassword,
  type AuthSession,
  type LinkedIdentity,
  type ListedPasskey,
  type ListedSession,
  type OidcProvider,
} from "@/lib/api";
import { assertPasskey, createPasskey } from "@/lib/webauthn";

/**
 * Account tab: Profile, Security, Sessions (and Owner elevation).
 */
export function AccountSettingsPanel({
  session,
  onSessionChange,
  onDeleted,
  sessions,
  sessionsBusy,
  sessionsError,
  onRefreshSessions,
  onRevokeSession,
  onRevokeOtherSessions,
}: {
  session: AuthSession | null;
  onSessionChange?: () => void | Promise<void>;
  onDeleted?: () => void | Promise<void>;
  sessions: ListedSession[];
  sessionsBusy: boolean;
  sessionsError: string | null;
  onRefreshSessions: () => void;
  onRevokeSession: (id: number) => void;
  onRevokeOtherSessions: () => void;
}) {
  const user = session?.user;
  const isOperatorOnly = session?.role === "operator" && !user;
  const canElevate =
    session?.role === "owner" && !session.impersonating && !session.elevated;
  const showElevationControls =
    (session?.role === "owner" && !session.impersonating) ||
    (session?.elevated === true && !session.impersonating);
  const [password, setPasswordValue] = useState("");
  const [passwordConfirm, setPasswordConfirm] = useState("");
  const [currentPassword, setCurrentPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [elevatePassword, setElevatePassword] = useState("");
  const [elevateBusy, setElevateBusy] = useState(false);
  const [deleteConfirm, setDeleteConfirm] = useState("");
  const [passkeys, setPasskeys] = useState<ListedPasskey[]>([]);
  const [identities, setIdentities] = useState<LinkedIdentity[]>([]);
  const [oidcProviders, setOidcProviders] = useState<OidcProvider[]>([]);

  useEffect(() => {
    const code = new URLSearchParams(window.location.search).get("sso_error");
    if (code === "mismatch") {
      setError("The identity provider account did not match this user.");
    }
  }, []);

  useEffect(() => {
    if (!user) return;
    let cancelled = false;
    void (async () => {
      try {
        const [keys, ids, oidc] = await Promise.all([
          listPasskeys(),
          listOidcIdentities(),
          listOidcProviders(),
        ]);
        if (cancelled) return;
        setPasskeys(keys);
        setIdentities(ids);
        setOidcProviders(oidc.providers);
      } catch {
        if (!cancelled) {
          setPasskeys([]);
          setIdentities([]);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [user?.id]);

  async function onChangePassword(e: FormEvent) {
    e.preventDefault();
    setError(null);
    setNotice(null);
    if (password.length < 8) {
      setError("Password must be at least 8 characters.");
      return;
    }
    if (password !== passwordConfirm) {
      setError("Passwords do not match.");
      return;
    }
    const needsCurrent = Boolean(user?.has_password);
    if (needsCurrent && !currentPassword) {
      setError("Enter your current password to change it.");
      return;
    }
    setBusy(true);
    try {
      const res = await setPassword({
        password,
        current_password: needsCurrent ? currentPassword : undefined,
      });
      setPasswordValue("");
      setPasswordConfirm("");
      setCurrentPassword("");
      setNotice(
        res.revoked_sessions > 0
          ? `Password updated. ${res.revoked_sessions} other session(s) were signed out.`
          : "Password updated.",
      );
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to update password");
    } finally {
      setBusy(false);
    }
  }

  async function onDeleteAccount() {
    if (!user) return;
    if (deleteConfirm.trim().toLowerCase() !== "delete") {
      setError('Type "delete" to confirm account deletion.');
      return;
    }
    if (
      !window.confirm(
        "Permanently delete your Bookclerk account? Acquired books stay on the host; your wishlist, sessions, and store links are removed.",
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteUser(user.id);
      await onDeleted?.();
    } catch (err) {
      if (isApiError(err) && err.status === 409) {
        setError("Cannot delete the last active owner.");
      } else {
        setError(err instanceof Error ? err.message : "Failed to delete account");
      }
      setBusy(false);
    }
  }

  async function onElevate(e: FormEvent) {
    e.preventDefault();
    setElevateBusy(true);
    setError(null);
    try {
      await elevate(elevatePassword);
      setElevatePassword("");
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Elevation failed");
    } finally {
      setElevateBusy(false);
    }
  }

  async function onEndElevate() {
    setElevateBusy(true);
    setError(null);
    try {
      await endElevate();
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to end elevation");
    } finally {
      setElevateBusy(false);
    }
  }

  async function onRegisterPasskey() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const begin = await passkeyRegisterBegin(
        passkeys.length > 0 ? currentPassword || undefined : undefined,
      );
      const result = await createPasskey(begin);
      await passkeyRegisterFinish(result);
      setPasskeys(await listPasskeys());
      setNotice("Passkey registered. You can use it to sign in if SSO is unavailable.");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Passkey registration failed");
    } finally {
      setBusy(false);
    }
  }

  async function onDeletePasskey(id: number) {
    setBusy(true);
    setError(null);
    try {
      await deletePasskey(id, currentPassword || undefined);
      setPasskeys(await listPasskeys());
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to remove passkey");
    } finally {
      setBusy(false);
    }
  }

  async function onElevatePasskey() {
    setElevateBusy(true);
    setError(null);
    try {
      const begin = await passkeyElevateBegin();
      const result = await assertPasskey(begin);
      await passkeyElevateFinish(result);
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Passkey elevation failed");
    } finally {
      setElevateBusy(false);
    }
  }

  const oidcIdentities = identities.filter((i) => i.provider.startsWith("oidc:"));
  const linkedProviderIds = new Set(
    oidcIdentities.map((i) => i.provider.replace(/^oidc:/, "")),
  );
  const elevateProviders = oidcProviders.filter((p) => linkedProviderIds.has(p.id));
  const showPasskeyBanner = Boolean(user) && oidcIdentities.length > 0 && passkeys.length === 0;
  const hasPassword = session?.user?.has_password === true;

  return (
    <div className="flex flex-col gap-10">
      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}
      {notice ? (
        <p className="text-sm font-medium text-teal" role="status">
          {notice}
        </p>
      ) : null}

      <section className="space-y-3">
        <div className="space-y-1">
          <h2 className="text-lg font-semibold text-ink">Profile</h2>
          <p className="text-sm text-ink/55">
            How this session identifies you on this Bookclerk host.
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-4 bg-white/35 px-4 py-4">
          <div className="relative flex h-14 w-14 items-center justify-center rounded-full bg-ink/10 text-lg font-semibold text-ink">
            {(user?.display_name || session?.role || "?")
              .trim()
              .charAt(0)
              .toUpperCase()}
            {session?.authenticated ? (
              <span
                className="absolute bottom-0.5 right-0.5 h-3 w-3 rounded-full border-2 border-paper bg-teal"
                title="Signed in"
              />
            ) : null}
          </div>
          <div className="min-w-0 space-y-1">
            <div className="flex flex-wrap items-center gap-2">
              <p className="font-semibold text-ink">
                {user?.display_name?.trim() ||
                  (isOperatorOnly ? "Operator" : "Account")}
              </p>
              <Badge className="bg-ink/10 text-ink normal-case tracking-normal">
                {session?.role ?? "unknown"}
              </Badge>
              {session?.elevated ? (
                <Badge className="bg-teal/15 text-ink normal-case tracking-normal">
                  Elevated
                </Badge>
              ) : null}
            </div>
            <p className="text-sm text-ink/55">
              {user
                ? `User #${user.id}`
                : isOperatorOnly
                  ? "Daemon operator token session"
                  : "No first-party user on this session"}
            </p>
          </div>
        </div>
      </section>

      <section className="space-y-3">
        <div className="space-y-1">
          <h2 className="text-lg font-semibold text-ink">Security</h2>
          <p className="text-sm text-ink/55">
            Password, passkeys, linked identity providers, and Owner elevation.
          </p>
        </div>

        {user ? (
          <>
            <form
              className="grid max-w-xl gap-3 bg-white/35 px-4 py-4 sm:grid-cols-2"
              onSubmit={(e) => void onChangePassword(e)}
            >
              {user?.has_password ? (
                <Input
                  className="sm:col-span-2"
                  type="password"
                  autoComplete="current-password"
                  placeholder="Current password"
                  aria-label="Current password"
                  value={currentPassword}
                  onChange={(e) => setCurrentPassword(e.target.value)}
                  disabled={busy}
                />
              ) : null}
              <Input
                type="password"
                autoComplete="new-password"
                placeholder="New password"
                aria-label="New password"
                value={password}
                onChange={(e) => setPasswordValue(e.target.value)}
                disabled={busy}
              />
              <Input
                type="password"
                autoComplete="new-password"
                placeholder="Confirm password"
                aria-label="Confirm password"
                value={passwordConfirm}
                onChange={(e) => setPasswordConfirm(e.target.value)}
                disabled={busy}
              />
              <div className="sm:col-span-2">
                <Button type="submit" disabled={busy || !password}>
                  <KeyRound className="h-4 w-4" />
                  {busy ? "Saving…" : "Update password"}
                </Button>
              </div>
            </form>
            {showPasskeyBanner ? (
              <div className="flex items-start gap-3 border border-teal/30 bg-teal/10 px-4 py-3 text-sm text-ink/70">
                <Fingerprint className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
                <div>
                  <p className="font-medium text-ink">Register a passkey</p>
                  <p>
                    SSO created this account. Add a passkey so you can still sign
                    in (and Owners can elevate) if the identity provider is down.
                  </p>
                </div>
              </div>
            ) : null}
            <div className="flex flex-col gap-3 bg-white/35 px-4 py-4">
              <div className="flex items-start gap-3 text-sm text-ink/70">
                <Fingerprint className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
                <div>
                  <p className="font-medium text-ink">Passkeys</p>
                  <p>
                    Phishing-resistant sign-in that stays on this Bookclerk host.
                  </p>
                </div>
              </div>
              {passkeys.length === 0 ? (
                <p className="text-sm text-ink/55">No passkeys registered.</p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {passkeys.map((pk) => (
                    <li
                      key={pk.id}
                      className="flex items-center justify-between gap-3 text-sm"
                    >
                      <span className="truncate font-mono text-ink/70">
                        {pk.credential_id.slice(0, 16)}…
                      </span>
                      <Button
                        type="button"
                        variant="secondary"
                        disabled={busy}
                        onClick={() => void onDeletePasskey(pk.id)}
                      >
                        Remove
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              <Button
                type="button"
                variant="secondary"
                disabled={busy}
                onClick={() => void onRegisterPasskey()}
              >
                <Fingerprint className="h-4 w-4" />
                {busy ? "Waiting…" : "Add passkey"}
              </Button>
            </div>
            {oidcIdentities.length > 0 ? (
              <div className="flex flex-col gap-2 bg-white/35 px-4 py-4">
                <p className="text-sm font-medium text-ink">Linked sign-in</p>
                <ul className="flex flex-wrap gap-1.5">
                  {oidcIdentities.map((id) => (
                    <li key={`${id.provider}:${id.external_user_id}`}>
                      <Badge className="bg-ink/8 text-ink normal-case tracking-normal">
                        {id.provider.replace(/^oidc:/, "")}
                        {id.label ? ` · ${id.label}` : ""}
                      </Badge>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </>
        ) : (
          <div className="space-y-2 bg-white/35 px-4 py-4 text-sm text-ink/60">
            <p>
              Operator-only sessions use the daemon token. Sign in as an owner,
              administrator, or member to manage a local password.
            </p>
          </div>
        )}

        {showElevationControls ? (
          <div className="space-y-3 border border-ink/10 bg-white/30 px-4 py-4">
            <div className="space-y-1">
              <h3 className="text-base font-semibold text-ink">
                Operator elevation
              </h3>
              <p className="text-sm text-ink/55">
                Owners can elevate to Operator with a fresh IdP login, a
                passkey, or a local password. Elevation unlocks Server Settings,
                Plugins, and impersonation. The Operator token is a separate
                local session, not elevation.
              </p>
            </div>
            {session?.elevated ? (
              <div className="flex flex-wrap items-center justify-between gap-3 bg-teal/10 px-3 py-3">
                <p className="text-sm text-ink">
                  Elevation active for this session.
                </p>
                <Button
                  type="button"
                  variant="secondary"
                  disabled={elevateBusy}
                  onClick={() => void onEndElevate()}
                >
                  {elevateBusy ? "Ending…" : "End elevation"}
                </Button>
              </div>
            ) : canElevate ? (
              <div className="flex flex-col gap-3">
                {elevateProviders.map((p) => (
                  <Button
                    key={p.id}
                    type="button"
                    variant="secondary"
                    disabled={elevateBusy}
                    onClick={() => {
                      window.location.href = `/api/auth/oidc/elevate?provider=${encodeURIComponent(p.id)}`;
                    }}
                  >
                    Continue with {p.name}
                  </Button>
                ))}
                {passkeys.length > 0 ? (
                  <Button
                    type="button"
                    variant="secondary"
                    disabled={elevateBusy}
                    onClick={() => void onElevatePasskey()}
                  >
                    <Fingerprint className="h-4 w-4" />
                    {elevateBusy ? "Waiting for passkey…" : "Elevate with passkey"}
                  </Button>
                ) : null}
                {hasPassword ? (
                  <form
                    className="flex flex-wrap gap-2"
                    onSubmit={(e) => void onElevate(e)}
                  >
                    <Input
                      className="min-w-64 flex-1"
                      type="password"
                      value={elevatePassword}
                      onChange={(e) => setElevatePassword(e.target.value)}
                      placeholder="Confirm your password"
                      autoComplete="current-password"
                      aria-label="Confirm password to elevate"
                    />
                    <Button
                      type="submit"
                      disabled={elevateBusy || !elevatePassword.trim()}
                    >
                      {elevateBusy ? "Elevating…" : "Elevate to Operator"}
                    </Button>
                  </form>
                ) : (
                  <p className="text-sm text-ink/55">
                    This Owner has no local password. Use SSO step-up or a
                    passkey, or sign in with the Operator token on the host.
                  </p>
                )}
              </div>
            ) : null}
          </div>
        ) : null}

        {user ? (
          <div className="max-w-xl space-y-3 border border-brick/20 bg-brick/5 px-4 py-4">
            <div className="space-y-1">
              <h3 className="text-base font-semibold text-brick">Delete account</h3>
              <p className="text-sm text-ink/55">
                Removes your wishlist, store links, and sessions. Acquired
                library titles remain on this host.
              </p>
            </div>
            <Input
              aria-label="Type delete to confirm"
              placeholder='Type "delete" to confirm'
              value={deleteConfirm}
              onChange={(e) => setDeleteConfirm(e.target.value)}
              disabled={busy}
              autoComplete="off"
            />
            <Button
              type="button"
              variant="danger"
              disabled={busy || deleteConfirm.trim().toLowerCase() !== "delete"}
              onClick={() => void onDeleteAccount()}
            >
              <Trash2 className="h-4 w-4" />
              Delete my account
            </Button>
          </div>
        ) : null}
      </section>

      <section className="space-y-3">
        <div className="space-y-1">
          <h2 className="text-lg font-semibold text-ink">Sessions</h2>
          <p className="text-sm text-ink/55">
            Devices signed in as you. Revoke any session you do not recognize.
          </p>
        </div>
        <SessionsPanel
          sessions={sessions}
          busy={sessionsBusy}
          error={sessionsError}
          onRefresh={onRefreshSessions}
          onRevoke={onRevokeSession}
          onRevokeOthers={onRevokeOtherSessions}
          embedded
        />
      </section>
    </div>
  );
}
