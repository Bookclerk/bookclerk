import { useEffect, useRef, useState, type FormEvent } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  checkPasswordReset,
  finishPasswordReset,
  isApiError,
  passwordResetPasskeyBegin,
  type ResetFactor,
} from "@/lib/api";
import { assertPasskey, passkeysSupported } from "@/lib/webauthn";

type Phase =
  | { kind: "checking" }
  | { kind: "form"; factor: ResetFactor }
  | { kind: "done" }
  /** `retry`: the check got no answer, so the same link may be tried again. */
  | { kind: "failed"; message: string; retry: boolean };

/** What a link that does not work says, whatever the reason. */
const INVALID_LINK =
  "This link is invalid or has expired. Ask for a new one from the sign-in page with Forgot password.";

/** What a request that got no answer says. */
const NO_ANSWER = "The library did not answer. Check your connection and try again.";

/**
 * `/reset-password?token=…`: uses a mailed password-reset link. The link is
 * the proof, so the page works without a sign-in. The token is removed from
 * the address bar before the first request, so it is not kept in history.
 *
 * The page checks the link, asks for the account's second factor when it
 * has one (an authenticator code or a passkey), then sets the new password.
 * Every device is signed out afterwards, so the page ends at sign-in.
 */
export function ResetPasswordPage() {
  const [phase, setPhase] = useState<Phase>({ kind: "checking" });
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // One check per page load (StrictMode runs effects twice).
  const checked = useRef(false);
  // The token, kept in memory only.
  const token = useRef<string | null>(null);

  function check(value: string) {
    setPhase({ kind: "checking" });
    void checkPasswordReset(value)
      .then((factor) => setPhase({ kind: "form", factor }))
      .catch((err: unknown) => {
        setPhase(
          isApiError(err) && err.status === 400
            ? { kind: "failed", message: INVALID_LINK, retry: false }
            : isApiError(err) && err.status === 429
              ? {
                  kind: "failed",
                  message:
                    "Too many attempts from this device. Wait a few minutes, then try again.",
                  retry: true,
                }
              : { kind: "failed", message: NO_ANSWER, retry: true },
        );
      });
  }

  useEffect(() => {
    if (checked.current) return;
    checked.current = true;
    const value = new URLSearchParams(window.location.search).get("token");
    window.history.replaceState(null, "", window.location.pathname);
    if (!value) {
      setPhase({
        kind: "failed",
        message: "This page needs the link from the email. Open it from the message again.",
        retry: false,
      });
      return;
    }
    token.current = value;
    check(value);
  }, []);

  async function onSubmit(e: FormEvent, factor: ResetFactor) {
    e.preventDefault();
    const value = token.current;
    if (!value) return;
    if (password.trim() !== confirm.trim()) {
      setError("The passwords do not match.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      if (factor === "passkey") {
        const begin = await passwordResetPasskeyBegin(value);
        const passkey = await assertPasskey(begin);
        await finishPasswordReset({ token: value, password, passkey });
      } else if (factor === "totp") {
        await finishPasswordReset({ token: value, password, totp_code: code.trim() });
      } else {
        await finishPasswordReset({ token: value, password });
      }
      setPhase({ kind: "done" });
    } catch (err) {
      if (isApiError(err) && err.code === "invalid_link") {
        setPhase({ kind: "failed", message: INVALID_LINK, retry: false });
      } else if (isApiError(err) && err.status === 401) {
        setError(
          factor === "passkey"
            ? "That passkey did not confirm this account. Try again."
            : "That code is not right. Enter the current code from your authenticator app.",
        );
      } else if (isApiError(err) && err.status === 429) {
        setError("Too many attempts from this device. Wait a few minutes, then try again.");
      } else if (isApiError(err) && err.status === 400) {
        setError(err.message || "Choose a password of at most 1024 characters.");
      } else if (err instanceof Error && err.name === "NotAllowedError") {
        setError("The passkey prompt was closed. Try again.");
      } else {
        setError(NO_ANSWER);
      }
    } finally {
      setBusy(false);
    }
  }

  const toSignIn = (
    <Button type="button" onClick={() => window.location.assign("/")}>
      Sign in
    </Button>
  );

  if (phase.kind === "checking") {
    return (
      <JoinShell title="Checking your link…" lead="Making sure the reset link still works.">
        <p className="text-sm text-ink/70">One moment.</p>
      </JoinShell>
    );
  }

  if (phase.kind === "failed") {
    return (
      <JoinShell title="Could not reset your password" lead={phase.message}>
        <div className="flex gap-2">
          {phase.retry && token.current ? (
            <Button
              type="button"
              onClick={() => {
                if (token.current) check(token.current);
              }}
            >
              Try again
            </Button>
          ) : null}
          <Button
            type="button"
            variant={phase.retry ? "secondary" : "primary"}
            onClick={() => window.location.assign("/")}
          >
            Open Bookclerk
          </Button>
        </div>
      </JoinShell>
    );
  }

  if (phase.kind === "done") {
    return (
      <JoinShell
        title="Password changed"
        lead="Every device that was signed in to your account has been signed out. Sign in with your new password."
      >
        <div className="flex gap-2">{toSignIn}</div>
      </JoinShell>
    );
  }

  const { factor } = phase;
  const passkeyBlocked = factor === "passkey" && !passkeysSupported();
  const ready =
    password.trim().length > 0 &&
    confirm.trim().length > 0 &&
    (factor !== "totp" || code.trim().length === 6) &&
    !passkeyBlocked;

  return (
    <JoinShell
      title="Choose a new password"
      lead="After it is set, every device signed in to your account is signed out."
    >
      <form onSubmit={(e) => void onSubmit(e, factor)} className="space-y-4">
        {error ? (
          <p className="text-sm font-medium text-brick-text" role="alert">
            {error}
          </p>
        ) : null}
        <div>
          <label className="block text-sm font-semibold" htmlFor="reset-password">
            New password
          </label>
          <Input
            id="reset-password"
            type="password"
            autoComplete="new-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            className="mt-1.5"
            required
          />
        </div>
        <div>
          <label className="block text-sm font-semibold" htmlFor="reset-password-confirm">
            Confirm new password
          </label>
          <Input
            id="reset-password-confirm"
            type="password"
            autoComplete="new-password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            className="mt-1.5"
            required
          />
        </div>
        {factor === "totp" ? (
          <div className="space-y-1.5 rounded-md border border-ink/10 bg-card-mid p-3">
            <label className="block text-sm font-semibold" htmlFor="reset-totp">
              Authenticator code
            </label>
            <p className="text-sm text-ink/70">
              Your account has an authenticator app. Enter its current 6-digit code.
            </p>
            <Input
              id="reset-totp"
              inputMode="numeric"
              autoComplete="one-time-code"
              value={code}
              onChange={(e) => setCode(e.target.value)}
              maxLength={6}
            />
          </div>
        ) : null}
        {factor === "passkey" ? (
          <p className="rounded-md border border-ink/10 bg-card-mid p-3 text-sm text-ink/70">
            {passkeyBlocked
              ? "Your account is protected by a passkey, and this browser cannot use passkeys. Open the link in a browser that can."
              : "Your account is protected by a passkey. Your browser asks for it when you save."}
          </p>
        ) : null}
        <Button type="submit" className="w-full" disabled={busy || !ready}>
          {busy
            ? factor === "passkey"
              ? "Waiting for passkey…"
              : "Saving…"
            : factor === "passkey"
              ? "Confirm with passkey and save"
              : "Save new password"}
        </Button>
      </form>
    </JoinShell>
  );
}
