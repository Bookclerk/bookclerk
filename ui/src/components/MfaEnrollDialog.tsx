import { useId, useState, type FormEvent } from "react";
import { Fingerprint, LogOut, Shield, Smartphone } from "lucide-react";
import { TotpSetupHint } from "@/components/TotpSetupHint";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  passkeyRegisterBegin,
  passkeyRegisterFinish,
  signOut,
  totpEnrollBegin,
  totpEnrollFinish,
  type AuthRole,
} from "@/lib/api";
import { createPasskey, passkeysSupported } from "@/lib/webauthn";

type Step = "choose" | "totp";

/**
 * Blocking gate when host MFA policy is on and this account has no passkey or TOTP.
 *
 * Setup or log out; logging out does not lock the account.
 */
export function MfaEnrollDialog({
  role,
  onEnrolled,
  onLoggedOut,
}: {
  role?: AuthRole;
  onEnrolled: () => void | Promise<void>;
  onLoggedOut: () => void;
}) {
  const titleId = useId();
  const canUsePasskeys = passkeysSupported();
  const [step, setStep] = useState<Step>("choose");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [passkeyName, setPasskeyName] = useState("");
  const [totpEnroll, setTotpEnroll] = useState<{
    secret: string;
    otpauth_url: string;
    qr_svg: string;
  } | null>(null);
  const [totpCode, setTotpCode] = useState("");

  async function onRegisterPasskey() {
    setBusy(true);
    setError(null);
    try {
      const begin = await passkeyRegisterBegin();
      const result = await createPasskey(begin);
      await passkeyRegisterFinish({
        ...result,
        name: passkeyName.trim() || undefined,
      });
      await onEnrolled();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Passkey registration failed");
    } finally {
      setBusy(false);
    }
  }

  async function onBeginTotp() {
    setBusy(true);
    setError(null);
    try {
      const begin = await totpEnrollBegin();
      setTotpEnroll(begin);
      setTotpCode("");
      setStep("totp");
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
      await onEnrolled();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid authenticator code");
    } finally {
      setBusy(false);
    }
  }

  async function onLogout() {
    setBusy(true);
    setError(null);
    try {
      await signOut(role);
    } catch {
      /* cookie clear is best-effort */
    }
    onLoggedOut();
  }

  return (
    <div className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-scrim px-4 py-10 sm:items-center">
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="w-full max-w-md rounded-lg border border-ink/10 bg-paper p-6 shadow-xl outline-none"
      >
        <div className="mb-4 flex items-start gap-3">
          <span className="mt-0.5 flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-brick/10 text-brick">
            <Shield className="h-4 w-4" aria-hidden />
          </span>
          <div>
            <h1 id={titleId} className="font-display text-xl font-semibold text-ink">
              Two-factor authentication required
            </h1>
            <p className="mt-1.5 text-sm text-ink/70">
              This host requires a passkey or authenticator app before you can
              use the library. Set one up now, or log out and finish later.
              Logging out does not lock your account.
            </p>
          </div>
        </div>

        {error ? (
          <p className="mb-4 text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}

        {step === "totp" && totpEnroll ? (
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
                  setStep("choose");
                  setTotpEnroll(null);
                  setTotpCode("");
                  setError(null);
                }}
              >
                Back
              </Button>
            </div>
          </form>
        ) : (
          <div className="flex flex-col gap-3">
            <label className="flex flex-col gap-1.5 text-sm font-medium text-ink">
              Passkey name
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
              disabled={busy || !canUsePasskeys}
              title={
                canUsePasskeys ? undefined : "This browser does not support passkeys"
              }
              onClick={() => void onRegisterPasskey()}
            >
              <Fingerprint className="h-4 w-4" aria-hidden />
              {busy ? "Waiting…" : "Set up passkey"}
            </Button>
            <Button
              type="button"
              variant="secondary"
              disabled={busy}
              onClick={() => void onBeginTotp()}
            >
              <Smartphone className="h-4 w-4" aria-hidden />
              Set up authenticator
            </Button>
          </div>
        )}

        <div className="mt-5 border-t border-ink/10 pt-4">
          <Button
            type="button"
            variant="ghost"
            className="w-full"
            disabled={busy}
            onClick={() => void onLogout()}
          >
            <LogOut className="h-4 w-4" aria-hidden />
            Log out
          </Button>
        </div>
      </div>
    </div>
  );
}
