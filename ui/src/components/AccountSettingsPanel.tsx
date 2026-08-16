import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent, type ReactNode } from "react";
import { Camera, Fingerprint, KeyRound, Pencil, RefreshCw, Shield, Smartphone, Trash2 } from "lucide-react";
import { SsoSignInButton } from "@/components/SsoProviderMark";
import { AvatarPickerDialog } from "@/components/AvatarPickerDialog";
import { SessionsPanel } from "@/components/SessionsPanel";
import { TotpSetupHint } from "@/components/TotpSetupHint";
import { UserAvatar, userDisplayLabel } from "@/components/UserAvatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  deletePasskey,
  deleteProfileAvatar,
  deleteUser,
  disableTotp,
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
  patchProfile,
  setPassword,
  totpEnrollBegin,
  totpEnrollFinish,
  uploadProfileAvatar,
  type AuthSession,
  type LinkedIdentity,
  type ListedPasskey,
  type ListedSession,
  type OidcProvider,
} from "@/lib/api";
import { assertPasskey, createPasskey, passkeysSupported } from "@/lib/webauthn";
import { isOptionalEmailValid } from "@/lib/email";
import { cn } from "@/lib/utils";

/**
 * Account tab: Profile, Security, Sessions (and Owner elevation).
 *
 * Security (password, passkeys, authenticator app, linked IdPs, delete) is omitted while an
 * operator is impersonating, because those controls belong to the target user.
 * Profile edits are also disabled while impersonating. Own-account Profile
 * defaults to the same rendered view, with hover edit controls for name,
 * email, and picture.
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
  const isImpersonating = Boolean(session?.impersonating);
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
  const [passkeyName, setPasskeyName] = useState("");
  const [totpEnabled, setTotpEnabled] = useState(false);
  const [totpEnroll, setTotpEnroll] = useState<{
    secret: string;
    otpauth_url: string;
    qr_svg: string;
  } | null>(null);
  const [totpCode, setTotpCode] = useState("");
  const [identities, setIdentities] = useState<LinkedIdentity[]>([]);
  const [oidcProviders, setOidcProviders] = useState<OidcProvider[]>([]);
  const [displayName, setDisplayName] = useState(user?.display_name ?? "");
  const [email, setEmail] = useState(user?.email ?? "");
  const [avatarKey, setAvatarKey] = useState(0);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [editingField, setEditingField] = useState<"name" | "email" | null>(
    null,
  );
  const skipProfileBlur = useRef(false);
  const canEditProfile = Boolean(user) && !isImpersonating;

  useEffect(() => {
    setDisplayName(user?.display_name ?? "");
    setEmail(user?.email ?? "");
    setEditingField(null);
  }, [user?.id, user?.display_name, user?.email]);

  useEffect(() => {
    const code = new URLSearchParams(window.location.search).get("sso_error");
    if (code === "mismatch") {
      setError("The identity provider account did not match this user.");
    }
  }, []);

  useEffect(() => {
    setTotpEnabled(Boolean(session?.second_factor?.totp));
  }, [session?.second_factor?.totp]);

  useEffect(() => {
    if (!user?.id || isImpersonating) return;
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
  }, [user?.id, isImpersonating]);

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
      await passkeyRegisterFinish({
        ...result,
        name: passkeyName.trim() || undefined,
      });
      setPasskeys(await listPasskeys());
      setPasskeyName("");
      setNotice("Passkey registered. You can use it to sign in if SSO is unavailable.");
      await onSessionChange?.();
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
      await onSessionChange?.();
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

  async function onBeginTotp() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const begin = await totpEnrollBegin(currentPassword || undefined);
      setTotpEnroll(begin);
      setTotpCode("");
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to start authenticator setup");
    } finally {
      setBusy(false);
    }
  }

  async function onConfirmTotp(e: FormEvent) {
    e.preventDefault();
    if (!totpEnroll) return;
    setBusy(true);
    setError(null);
    try {
      await totpEnrollFinish(totpCode.trim());
      setTotpEnroll(null);
      setTotpCode("");
      setTotpEnabled(true);
      setNotice("Authenticator app enabled. You will need a code when signing in with a password.");
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid authenticator code");
    } finally {
      setBusy(false);
    }
  }

  async function onDisableTotp() {
    setBusy(true);
    setError(null);
    try {
      await disableTotp(currentPassword || undefined);
      setTotpEnabled(false);
      setTotpEnroll(null);
      setNotice("Authenticator app disabled.");
      await onSessionChange?.();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to disable authenticator");
    } finally {
      setBusy(false);
    }
  }

  function cancelProfileEdit() {
    skipProfileBlur.current = true;
    setDisplayName(user?.display_name ?? "");
    setEmail(user?.email ?? "");
    setEditingField(null);
  }

  async function commitDisplayName() {
    if (!canEditProfile) return;
    const next = displayName.trim();
    const prev = (user?.display_name ?? "").trim();
    if (next === prev) {
      setDisplayName(user?.display_name ?? "");
      setEditingField(null);
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await patchProfile({ display_name: next });
      await onSessionChange?.();
      setEditingField(null);
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to save display name");
    } finally {
      setBusy(false);
    }
  }

  async function commitEmail() {
    if (!canEditProfile) return;
    if (!isOptionalEmailValid(email)) {
      setError("Enter a valid email address.");
      return;
    }
    const next = email.trim();
    const prev = (user?.email ?? "").trim();
    if (next === prev) {
      setEmail(user?.email ?? "");
      setEditingField(null);
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await patchProfile({ email: next });
      await onSessionChange?.();
      setEditingField(null);
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to save email");
    } finally {
      setBusy(false);
    }
  }

  function onProfileFieldKey(e: KeyboardEvent<HTMLInputElement>) {
    if (e.key === "Escape") {
      e.preventDefault();
      cancelProfileEdit();
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault();
      e.currentTarget.blur();
    }
  }

  async function onPickAvatar(file: File | undefined) {
    if (!file || !canEditProfile) return;
    if (file.size > 1_500_000) {
      setError("Choose an image smaller than 1.5 MB.");
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await uploadProfileAvatar(file);
      setAvatarKey(Date.now());
      await onSessionChange?.();
      setNotice("Profile picture updated.");
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to update profile picture");
    } finally {
      setBusy(false);
    }
  }

  async function onSelectAvatarSource(source: string) {
    if (!canEditProfile) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await patchProfile({ avatar_source: source });
      await onSessionChange?.();
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to update profile picture");
    } finally {
      setBusy(false);
    }
  }

  async function onRemoveAvatar() {
    if (!canEditProfile) return;
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await deleteProfileAvatar();
      setAvatarKey(Date.now());
      await onSessionChange?.();
      setNotice("Profile picture removed.");
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to remove profile picture");
    } finally {
      setBusy(false);
    }
  }

  const oidcIdentities = identities.filter((i) => i.provider.startsWith("oidc:"));
  const linkedProviderIds = new Set(
    oidcIdentities.map((i) => i.provider.replace(/^oidc:/, "")),
  );
  const elevateProviders = oidcProviders.filter((p) => linkedProviderIds.has(p.id));
  const showPasskeyBanner = Boolean(user) && oidcIdentities.length > 0 && passkeys.length === 0;
  const canUsePasskeys = passkeysSupported();
  const hasPassword = session?.user?.has_password === true;
  const profileLabel = isOperatorOnly
    ? "Operator"
    : userDisplayLabel({
        display_name: displayName || user?.display_name,
        email: email || user?.email,
        login_name: user?.login_name,
      });

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
        <div className="group/profile flex items-center gap-4 bg-card px-4 py-4">
          <button
            type="button"
            className="group relative shrink-0 rounded-full focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal disabled:cursor-default"
            disabled={!canEditProfile || busy}
            onClick={() => setPickerOpen(true)}
            aria-label={
              canEditProfile ? "Change profile picture" : "Profile picture"
            }
          >
            <UserAvatar
              userId={user?.id}
              label={profileLabel}
              hasAvatar={user?.has_avatar}
              avatarSource={user?.avatar_source}
              gravatarHash={user?.gravatar_hash}
              ssoPictures={user?.sso_pictures}
              cacheKey={avatarKey}
              className="h-14 w-14 text-lg"
            >
              {session?.authenticated ? (
                <span
                  className="absolute bottom-0.5 right-0.5 h-3 w-3 rounded-full border-2 border-paper bg-teal"
                  title="Signed in"
                />
              ) : null}
              {canEditProfile ? (
                <span className="absolute inset-0 flex items-center justify-center rounded-full bg-ink/45 text-paper opacity-0 transition-opacity group-hover/profile:opacity-100 group-focus-within/profile:opacity-100">
                  <Camera className="h-5 w-5" aria-hidden />
                </span>
              ) : null}
            </UserAvatar>
          </button>
          <div className="min-w-0 flex-1 space-y-0.5">
            <div className="flex min-w-0 flex-wrap items-center gap-2">
              {editingField === "name" ? (
                <Input
                  className="max-w-xs"
                  autoFocus
                  value={displayName}
                  onChange={(e) => setDisplayName(e.target.value)}
                  onBlur={() => {
                    if (skipProfileBlur.current) {
                      skipProfileBlur.current = false;
                      return;
                    }
                    void commitDisplayName();
                  }}
                  onKeyDown={onProfileFieldKey}
                  maxLength={80}
                  autoComplete="nickname"
                  placeholder="Display name"
                  aria-label="Display name"
                  disabled={busy}
                />
              ) : canEditProfile ? (
                <button
                  type="button"
                  className="flex min-w-0 items-center gap-1.5 rounded-sm text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal"
                  onClick={() => setEditingField("name")}
                  aria-label="Edit display name"
                >
                  <span className="truncate font-semibold text-ink">
                    {profileLabel}
                  </span>
                  <Pencil
                    className="h-3.5 w-3.5 shrink-0 text-ink/40 opacity-0 transition-opacity group-hover/profile:opacity-100 group-focus-within/profile:opacity-100"
                    aria-hidden
                  />
                </button>
              ) : (
                <p className="truncate font-semibold text-ink">{profileLabel}</p>
              )}
              <Badge className="bg-ink/10 text-ink normal-case tracking-normal">
                {session?.role ?? "unknown"}
              </Badge>
              {session?.elevated ? (
                <Badge className="bg-teal/15 text-ink normal-case tracking-normal">
                  Elevated
                </Badge>
              ) : null}
            </div>
            {editingField === "email" ? (
              <Input
                className="max-w-sm"
                type="email"
                autoFocus
                value={email}
                aria-invalid={!isOptionalEmailValid(email)}
                onChange={(e) => setEmail(e.target.value)}
                onBlur={() => {
                  if (skipProfileBlur.current) {
                    skipProfileBlur.current = false;
                    return;
                  }
                  void commitEmail();
                }}
                onKeyDown={onProfileFieldKey}
                autoComplete="email"
                placeholder="you@example.com"
                aria-label="Email"
                disabled={busy}
              />
            ) : canEditProfile ? (
              <button
                type="button"
                className="flex min-w-0 items-center gap-1.5 rounded-sm text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal"
                onClick={() => setEditingField("email")}
                aria-label="Edit email"
              >
                <span
                  className={cn(
                    "truncate text-sm",
                    email.trim() ? "text-ink/55" : "text-ink/40",
                  )}
                >
                  {email.trim() || "Add email"}
                </span>
                <Pencil
                  className="h-3.5 w-3.5 shrink-0 text-ink/40 opacity-0 transition-opacity group-hover/profile:opacity-100 group-focus-within/profile:opacity-100"
                  aria-hidden
                />
              </button>
            ) : (
              <p className="truncate text-sm text-ink/55">
                {user?.email?.trim() ||
                  (isOperatorOnly
                    ? "Daemon operator token session"
                    : "No email")}
              </p>
            )}
          </div>
        </div>
      </section>

      {!isImpersonating ? (
      <section className="flex flex-col gap-4">
        <div className="flex flex-col gap-1">
          <h2 className="text-lg font-semibold text-ink">Security</h2>
          <p className="text-sm text-ink/55">
            Password, passkeys, authenticator app, linked identity providers, and Owner elevation.
          </p>
        </div>

        {user && session?.second_factor?.required && !session.second_factor.enrolled ? (
          <div className="rounded-md border border-brick/30 bg-brick/10 px-3 py-2.5 text-sm text-ink/80">
            This host requires a passkey or authenticator app. Add one below to keep using the library.
          </div>
        ) : null}

        {user ? (
          <div className="grid gap-4 lg:grid-cols-2">
            <SecurityCard
              icon={<KeyRound className="h-4 w-4" aria-hidden />}
              title="Password"
              description="Sign in with a Bookclerk password on this host."
            >
              <form
                className="flex flex-col gap-3"
                onSubmit={(e) => void onChangePassword(e)}
              >
                {user.has_password ? (
                  <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                    Current password
                    <Input
                      type="password"
                      autoComplete="current-password"
                      value={currentPassword}
                      onChange={(e) => setCurrentPassword(e.target.value)}
                      disabled={busy}
                    />
                  </label>
                ) : null}
                <div className="grid gap-3 sm:grid-cols-2">
                  <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                    New password
                    <Input
                      type="password"
                      autoComplete="new-password"
                      value={password}
                      onChange={(e) => setPasswordValue(e.target.value)}
                      disabled={busy}
                    />
                  </label>
                  <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                    Confirm
                    <Input
                      type="password"
                      autoComplete="new-password"
                      value={passwordConfirm}
                      onChange={(e) => setPasswordConfirm(e.target.value)}
                      disabled={busy}
                    />
                  </label>
                </div>
                <Button
                  type="submit"
                  className="self-start"
                  disabled={busy || !password}
                >
                  <KeyRound className="h-4 w-4" />
                  {busy
                    ? "Saving…"
                    : user.has_password
                      ? "Update password"
                      : "Set password"}
                </Button>
              </form>
            </SecurityCard>

            <SecurityCard
              icon={<Fingerprint className="h-4 w-4" aria-hidden />}
              title="Passkeys"
              description="Phishing-resistant sign-in that stays on this Bookclerk host."
            >
              {showPasskeyBanner ? (
                <div className="rounded-md border border-teal/30 bg-teal/10 px-3 py-2.5 text-sm text-ink/70">
                  <p className="font-medium text-ink">Register a passkey</p>
                  <p className="mt-0.5">
                    SSO created this account. Add a passkey so you can still
                    sign in (and Owners can elevate) if the identity provider
                    is down.
                  </p>
                </div>
              ) : null}
              {passkeys.length === 0 ? (
                <p className="text-sm text-ink/55">No passkeys registered.</p>
              ) : (
                <ul className="flex flex-col gap-2">
                  {passkeys.map((pk) => (
                    <li
                      key={pk.id}
                      className="flex items-center justify-between gap-3 rounded-md border border-ink/10 bg-card-mid px-3 py-2 text-sm"
                    >
                      <span className="min-w-0">
                        <span className="block truncate font-medium text-ink">
                          {pk.name?.trim() || "Passkey"}
                        </span>
                        <span className="block truncate font-mono text-xs text-ink/45">
                          {pk.credential_id.slice(0, 16)}…
                        </span>
                      </span>
                      <Button
                        type="button"
                        variant="ghost"
                        disabled={busy}
                        onClick={() => void onDeletePasskey(pk.id)}
                      >
                        Remove
                      </Button>
                    </li>
                  ))}
                </ul>
              )}
              <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                Name
                <Input
                  value={passkeyName}
                  onChange={(e) => setPasskeyName(e.target.value)}
                  maxLength={80}
                  placeholder="Laptop, YubiKey, …"
                  disabled={busy}
                />
              </label>
              <Button
                type="button"
                variant="secondary"
                className="self-start"
                disabled={busy || !canUsePasskeys}
                title={
                  canUsePasskeys
                    ? undefined
                    : "This browser does not support passkeys"
                }
                onClick={() => void onRegisterPasskey()}
              >
                <Fingerprint className="h-4 w-4" />
                {busy ? "Waiting…" : "Add passkey"}
              </Button>
              {oidcIdentities.length > 0 ? (
                <div className="flex flex-col gap-2 border-t border-ink/10 pt-3">
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
            </SecurityCard>

            <SecurityCard
              icon={<Smartphone className="h-4 w-4" aria-hidden />}
              title="Authenticator app"
              description="Time-based codes (TOTP) for password sign-in. Passkey sign-in does not need a code."
            >
              {totpEnabled ? (
                <>
                  <p className="text-sm text-ink/70">
                    Authenticator app is enabled for this account.
                  </p>
                  <Button
                    type="button"
                    variant="secondary"
                    className="self-start"
                    disabled={busy}
                    onClick={() => void onDisableTotp()}
                  >
                    Disable authenticator
                  </Button>
                </>
              ) : totpEnroll ? (
                <form className="flex flex-col gap-3" onSubmit={(e) => void onConfirmTotp(e)}>
                  <TotpSetupHint
                    secret={totpEnroll.secret}
                    otpauthUrl={totpEnroll.otpauth_url}
                    qrSvg={totpEnroll.qr_svg}
                  />
                  <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                    Authenticator code
                    <Input
                      inputMode="numeric"
                      autoComplete="one-time-code"
                      value={totpCode}
                      onChange={(e) => setTotpCode(e.target.value)}
                      maxLength={8}
                      disabled={busy}
                    />
                  </label>
                  <div className="flex flex-wrap gap-2">
                    <Button type="submit" disabled={busy || totpCode.trim().length < 6}>
                      Confirm
                    </Button>
                    <Button
                      type="button"
                      variant="ghost"
                      disabled={busy}
                      onClick={() => {
                        setTotpEnroll(null);
                        setTotpCode("");
                      }}
                    >
                      Cancel
                    </Button>
                  </div>
                </form>
              ) : (
                <Button
                  type="button"
                  variant="secondary"
                  className="self-start"
                  disabled={busy}
                  onClick={() => void onBeginTotp()}
                >
                  <Smartphone className="h-4 w-4" />
                  Set up authenticator
                </Button>
              )}
            </SecurityCard>

            {showElevationControls ? (
              <SecurityCard
                icon={<Shield className="h-4 w-4" aria-hidden />}
                title="Operator elevation"
                description="Unlocks Server Settings, Plugins, and impersonation for this session. The Operator token is a separate local login, not elevation."
              >
                {session?.elevated ? (
                  <div className="flex flex-wrap items-center justify-between gap-3 rounded-md bg-teal/10 px-3 py-2.5">
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
                    {elevateProviders.length > 0 || passkeys.length > 0 ? (
                      <div className="flex flex-wrap gap-2">
                        {elevateProviders.map((p) => (
                          <SsoSignInButton
                            key={p.id}
                            preset={p.preset}
                            name={p.name}
                            disabled={elevateBusy}
                            onClick={() => {
                              window.location.href = `/api/auth/oidc/elevate?provider=${encodeURIComponent(p.id)}`;
                            }}
                          />
                        ))}
                        {passkeys.length > 0 ? (
                          <Button
                            type="button"
                            variant="secondary"
                            disabled={elevateBusy || !canUsePasskeys}
                            title={
                              canUsePasskeys
                                ? undefined
                                : "This browser does not support passkeys — confirm your password instead"
                            }
                            onClick={() => void onElevatePasskey()}
                          >
                            <Fingerprint className="h-4 w-4" />
                            {elevateBusy
                              ? "Waiting for passkey…"
                              : "Elevate with passkey"}
                          </Button>
                        ) : null}
                      </div>
                    ) : null}
                    {hasPassword ? (
                      <form
                        className="flex flex-col gap-3"
                        onSubmit={(e) => void onElevate(e)}
                      >
                        <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                          Confirm password
                          <Input
                            type="password"
                            value={elevatePassword}
                            onChange={(e) => setElevatePassword(e.target.value)}
                            autoComplete="current-password"
                            disabled={elevateBusy}
                          />
                        </label>
                        <Button
                          type="submit"
                          className="self-start"
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
              </SecurityCard>
            ) : null}

            <SecurityCard
              icon={<Trash2 className="h-4 w-4" aria-hidden />}
              title="Delete account"
              description="Removes your wishlist, store links, and sessions. Acquired library titles remain on this host."
              className="border-brick/20 bg-brick/5"
              titleClassName="text-brick"
              iconClassName="text-brick/70"
            >
              <div className="flex flex-col gap-3">
                <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
                  Type delete to confirm
                  <Input
                    placeholder='Type "delete"'
                    value={deleteConfirm}
                    onChange={(e) => setDeleteConfirm(e.target.value)}
                    disabled={busy}
                    autoComplete="off"
                  />
                </label>
                <Button
                  type="button"
                  variant="danger"
                  className="self-start"
                  disabled={busy || deleteConfirm.trim().toLowerCase() !== "delete"}
                  onClick={() => void onDeleteAccount()}
                >
                  <Trash2 className="h-4 w-4" />
                  Delete my account
                </Button>
              </div>
            </SecurityCard>
          </div>
        ) : (
          <div className="max-w-xl rounded-md border border-ink/10 bg-card p-4 text-sm text-ink/60">
            <p>
              Operator-only sessions use the daemon token. Sign in as an owner,
              administrator, or member to manage a local password.
            </p>
          </div>
        )}
      </section>
      ) : null}

      <section className="space-y-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="space-y-1">
            <h2 className="text-lg font-semibold text-ink">Sessions</h2>
            <p className="text-sm text-ink/55">
              Devices signed in as you. Revoke any session you do not recognize.
            </p>
          </div>
          <Button
            type="button"
            variant="secondary"
            disabled={sessionsBusy}
            onClick={onRefreshSessions}
          >
            <RefreshCw className="h-4 w-4" />
            Refresh
          </Button>
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
      {user?.id && canEditProfile ? (
        <AvatarPickerDialog
          open={pickerOpen}
          onOpenChange={setPickerOpen}
          userId={user.id}
          label={profileLabel}
          fields={{
            has_avatar: user.has_avatar,
            avatar_source: user.avatar_source,
            gravatar_hash: user.gravatar_hash,
            sso_pictures: user.sso_pictures,
          }}
          cacheKey={avatarKey}
          busy={busy}
          onSelectSource={onSelectAvatarSource}
          onUpload={(file) => void onPickAvatar(file)}
          onRemoveUpload={() => void onRemoveAvatar()}
        />
      ) : null}
    </div>
  );
}

/**
 * One Security subsection (password, passkeys, authenticator, elevation, or delete).
 *
 * @param props - Icon, title, description, and card body.
 * @returns A bordered card with a consistent header and the subsection body.
 */
function SecurityCard({
  icon,
  title,
  description,
  children,
  className,
  titleClassName,
  iconClassName,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  children: ReactNode;
  className?: string;
  titleClassName?: string;
  iconClassName?: string;
}) {
  return (
    <div
      className={cn(
        "flex h-full flex-col gap-4 rounded-md border border-ink/10 bg-card p-4",
        className,
      )}
    >
      <div className="flex items-start gap-3">
        <span className={cn("mt-0.5 shrink-0 text-ink/50", iconClassName)}>
          {icon}
        </span>
        <div className="min-w-0 flex-1">
          <h3 className={cn("text-sm font-semibold text-ink", titleClassName)}>
            {title}
          </h3>
          <p className="mt-0.5 text-sm text-ink/55">{description}</p>
        </div>
      </div>
      {children}
    </div>
  );
}
