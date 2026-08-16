import { useEffect, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  authMe,
  fetchSigninMethods,
  login,
  passkeyLoginBegin,
  passkeyLoginFinish,
  passwordLogin,
  portalLoginIntegration,
  portalRedeem,
  totpLogin,
  type AuthSession,
  type OidcProvider,
} from "@/lib/api";
import { BookclerkLogo } from "@/components/BookclerkLogo";
import { SsoSignInButton } from "@/components/SsoProviderMark";
import { ThemePreferenceControl, useTheme } from "@/components/ThemeProvider";
import { dropTicketUnlessInvitePath, isInvitePath } from "@/lib/routes";
import { assertPasskey, passkeysSupported } from "@/lib/webauthn";

function ssoErrorMessage(code: string | null): string | null {
  switch (code) {
    case "denied":
      return "Sign-in was cancelled or denied by the identity provider.";
    case "expired":
      return "Sign-in expired. Try again.";
    case "nonce":
      return "Sign-in could not be verified. Try again.";
    case "no_role":
      return "Your account is not allowed to use this sign-in method.";
    case "conflict":
      return "This identity is already linked to another user.";
    case "disabled":
      return "This account is disabled.";
    case "mismatch":
      return "The identity provider account did not match this user.";
    default:
      return code ? "Sign-in failed." : null;
  }
}

/** Claim tickets are honored only on `/invite?ticket=`, never on `/discover`. */
function inviteTicket(): string {
  if (!isInvitePath()) return "";
  return new URLSearchParams(window.location.search).get("ticket") ?? "";
}

/** Prefer a real login error; branded empty 401s used to say "operator token". */
function loginFailureMessage(err: unknown, fallback: string): string {
  if (!(err instanceof Error) || !err.message.trim()) return fallback;
  const message = err.message.trim();
  if (/operator token/i.test(message)) return fallback;
  return message;
}

/**
 * Unauthenticated entry — password by default, SSO/integration buttons, claim via magic link.
 *
 * Operator-token paste is offered only before an Owner exists. Tray handoff
 * still signs the operator in without this form.
 *
 * @param props - Called with the new session after successful sign-in.
 */
export function LoginPage({
  onSuccess,
}: {
  onSuccess: (session: AuthSession) => void;
}) {
  const [ticket] = useState(() => {
    dropTicketUnlessInvitePath();
    return inviteTicket();
  });
  const claimMode = Boolean(ticket.trim());
  const [claimPassword, setClaimPassword] = useState("");
  const [claimPasswordConfirm, setClaimPasswordConfirm] = useState("");
  const [token, setToken] = useState("");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [totpChallengeId, setTotpChallengeId] = useState<string | null>(null);
  const [totpCode, setTotpCode] = useState("");
  const [integrationId, setIntegrationId] = useState<string | null>(null);
  const [integrationUser, setIntegrationUser] = useState("");
  const [integrationPassword, setIntegrationPassword] = useState("");
  const [error, setError] = useState<string | null>(() =>
    ssoErrorMessage(new URLSearchParams(window.location.search).get("sso_error")),
  );
  const [busy, setBusy] = useState(false);
  const [operatorToken, setOperatorToken] = useState(false);
  const [oidc, setOidc] = useState<OidcProvider[]>([]);
  const [integrations, setIntegrations] = useState<{ id: string; name: string }[]>(
    [],
  );

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const methods = await fetchSigninMethods();
        if (cancelled) return;
        setOperatorToken(methods.operator_token);
        setOidc(methods.oidc);
        setIntegrations(methods.integrations);
      } catch {
        if (!cancelled) {
          setOperatorToken(false);
          setOidc([]);
          setIntegrations([]);
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function finishPortal() {
    const session = await authMe();
    if (!session.authenticated) {
      throw new Error("Signed in but session was not established.");
    }
    onSuccess(session);
  }

  async function onOperatorSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const session = await login(token.trim());
      onSuccess(session);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid operator token.");
    } finally {
      setBusy(false);
    }
  }

  async function onClaimSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const nextPassword = claimPassword.trim();
    const confirm = claimPasswordConfirm.trim();
    if (nextPassword && nextPassword !== confirm) {
      setError("Passwords do not match.");
      setBusy(false);
      return;
    }
    try {
      await portalRedeem(ticket.trim(), nextPassword || undefined);
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid or expired ticket.");
    } finally {
      setBusy(false);
    }
  }

  async function onPasswordSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      if (totpChallengeId) {
        await totpLogin(totpChallengeId, totpCode.trim());
        await finishPortal();
        return;
      }
      const result = await passwordLogin(username.trim(), password);
      if (result.mfa?.method === "totp" && result.mfa.challenge_id) {
        setTotpChallengeId(result.mfa.challenge_id);
        setTotpCode("");
        return;
      }
      await finishPortal();
    } catch (err) {
      setError(
        loginFailureMessage(
          err,
          totpChallengeId
            ? "Invalid authenticator code."
            : "Invalid login or password.",
        ),
      );
    } finally {
      setBusy(false);
    }
  }

  async function onPasskeySubmit() {
    setBusy(true);
    setError(null);
    try {
      const begin = await passkeyLoginBegin(username.trim());
      const assertion = await assertPasskey(begin);
      await passkeyLoginFinish(assertion);
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Passkey sign-in failed.");
    } finally {
      setBusy(false);
    }
  }

  async function onIntegrationSubmit(e: FormEvent) {
    e.preventDefault();
    if (!integrationId) return;
    setBusy(true);
    setError(null);
    try {
      await portalLoginIntegration({
        provider: integrationId,
        username: integrationUser.trim(),
        password: integrationPassword,
      });
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Sign-in failed.");
    } finally {
      setBusy(false);
    }
  }

  const otherMethods = oidc.length > 0 || integrations.length > 0;
  const canUsePasskeys = passkeysSupported();
  const { preference: themePref, setPreference: setThemePreference } = useTheme();

  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <div className="w-full max-w-md animate-[fadeUp_420ms_ease-out]">
        <BookclerkLogo className="mb-8 h-12 w-auto" />
        <h1 className="font-display text-3xl font-bold tracking-tight text-ink">
          {claimMode ? "Accept invite" : "Sign in"}
        </h1>
        <p className="mt-2 text-sm text-ink/70">
          {claimMode
            ? "Set a password if this account does not have one yet, then continue."
            : operatorToken
              ? "This host has no Owner yet. Paste the operator token to finish setup, or sign in if you already have an account."
              : "Sign in with your Bookclerk username and password."}
        </p>

        {error ? (
          <p className="mt-4 text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}

        {claimMode ? (
          <form onSubmit={onClaimSubmit} className="mt-6">
            <label className="block text-sm font-semibold" htmlFor="claim-password">
              Password <span className="font-normal text-ink/55">(optional)</span>
            </label>
            <Input
              id="claim-password"
              type="password"
              value={claimPassword}
              onChange={(e) => setClaimPassword(e.target.value)}
              className="mt-1.5"
              autoComplete="new-password"
            />
            <label
              className="mt-4 block text-sm font-semibold"
              htmlFor="claim-password-confirm"
            >
              Confirm password
            </label>
            <Input
              id="claim-password-confirm"
              type="password"
              value={claimPasswordConfirm}
              onChange={(e) => setClaimPasswordConfirm(e.target.value)}
              className="mt-1.5"
              autoComplete="new-password"
            />
            <Button
              type="submit"
              className="mt-5 w-full"
              disabled={busy || !ticket.trim()}
            >
              {busy ? "Redeeming…" : "Continue"}
            </Button>
          </form>
        ) : (
          <>
            {operatorToken ? (
              <form onSubmit={onOperatorSubmit} className="mt-6">
                <label className="block text-sm font-semibold" htmlFor="token">
                  Operator token
                </label>
                <Input
                  id="token"
                  type="password"
                  autoComplete="current-password"
                  value={token}
                  onChange={(e) => setToken(e.target.value)}
                  className="mt-1.5"
                  placeholder="64-character hex token"
                  required
                />
                <Button
                  type="submit"
                  className="mt-5 w-full"
                  disabled={busy || !token}
                >
                  {busy ? "Signing in…" : "Open library"}
                </Button>
              </form>
            ) : null}

            <form onSubmit={onPasswordSubmit} className={operatorToken ? "mt-8" : "mt-6"}>
              {operatorToken ? (
                <p className="mb-4 text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Already have an account
                </p>
              ) : null}
              <label className="block text-sm font-semibold" htmlFor="local-login">
                Username
              </label>
              <Input
                id="local-login"
                autoComplete="username"
                value={username}
                onChange={(e) => setUsername(e.target.value)}
                className="mt-1.5"
                required
              />
              <label className="mt-4 block text-sm font-semibold" htmlFor="local-password">
                Password
              </label>
              <Input
                id="local-password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                className="mt-1.5"
                required
              />
              {totpChallengeId ? (
                <div className="mt-4 space-y-3 rounded-md border border-ink/10 bg-card-mid p-3">
                  <p className="text-sm text-ink/70">
                    Enter the 6-digit code from your authenticator app.
                  </p>
                  <label className="block text-sm font-semibold" htmlFor="totp-code">
                    Authenticator code
                  </label>
                  <Input
                    id="totp-code"
                    inputMode="numeric"
                    autoComplete="one-time-code"
                    value={totpCode}
                    onChange={(e) => setTotpCode(e.target.value)}
                    maxLength={8}
                    className="mt-1.5"
                  />
                </div>
              ) : null}
              <Button
                type="submit"
                className="mt-5 w-full"
                disabled={
                  busy ||
                  (totpChallengeId
                    ? totpCode.trim().length < 6
                    : !username || !password)
                }
              >
                {busy
                  ? totpChallengeId
                    ? "Verifying…"
                    : "Signing in…"
                  : totpChallengeId
                    ? "Continue"
                    : "Sign in"}
              </Button>
              <Button
                type="button"
                variant="secondary"
                className="mt-2 w-full"
                disabled={busy || !username.trim() || !canUsePasskeys}
                title={
                  canUsePasskeys
                    ? undefined
                    : "This browser does not support passkeys — use a password instead"
                }
                onClick={() => void onPasskeySubmit()}
              >
                {busy ? "Waiting for passkey…" : "Sign in with passkey"}
              </Button>
            </form>

            {otherMethods ? (
              <div className="mt-8">
                <p className="mb-3 text-center text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Or continue with
                </p>
                <div className="flex flex-col gap-2">
                  {oidc.map((p) => (
                    <SsoSignInButton
                      key={p.id}
                      preset={p.preset}
                      name={p.name}
                      className="w-full"
                      disabled={busy}
                      onClick={() => {
                        window.location.href = `/api/auth/oidc/login?provider=${encodeURIComponent(p.id)}`;
                      }}
                    />
                  ))}
                  {integrations.map((p) => (
                    <Button
                      key={p.id}
                      type="button"
                      variant="secondary"
                      className="w-full"
                      disabled={busy}
                      aria-expanded={integrationId === p.id}
                      onClick={() => {
                        setError(null);
                        setIntegrationId((cur) => (cur === p.id ? null : p.id));
                      }}
                    >
                      Continue with {p.name}
                    </Button>
                  ))}
                </div>
                {integrationId ? (
                  <form onSubmit={onIntegrationSubmit} className="mt-4">
                    <label
                      className="block text-sm font-semibold"
                      htmlFor="integration-username"
                    >
                      Username
                    </label>
                    <Input
                      id="integration-username"
                      value={integrationUser}
                      onChange={(e) => setIntegrationUser(e.target.value)}
                      className="mt-1.5"
                      autoComplete="username"
                      required
                    />
                    <label
                      className="mt-3 block text-sm font-semibold"
                      htmlFor="integration-password"
                    >
                      Password
                    </label>
                    <Input
                      id="integration-password"
                      type="password"
                      value={integrationPassword}
                      onChange={(e) => setIntegrationPassword(e.target.value)}
                      className="mt-1.5"
                      autoComplete="current-password"
                      required
                    />
                    <Button
                      type="submit"
                      className="mt-4 w-full"
                      disabled={
                        busy || !integrationUser.trim() || !integrationPassword
                      }
                    >
                      {busy ? "Signing in…" : "Sign in"}
                    </Button>
                  </form>
                ) : null}
              </div>
            ) : null}
          </>
        )}
        <div className="mt-8 border-t border-ink/10 pt-4">
          <p className="mb-2 text-xs font-semibold uppercase tracking-wide text-ink/45">
            Appearance
          </p>
          <ThemePreferenceControl
            compact
            value={themePref}
            onChange={setThemePreference}
          />
        </div>
      </div>
      <style>{`
        @keyframes fadeUp {
          from { opacity: 0; transform: translateY(10px); }
          to { opacity: 1; transform: translateY(0); }
        }
      `}</style>
    </div>
  );
}
