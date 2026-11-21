import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  downloadRecoveryCopy,
  fetchMasterKeyStatus,
  makeRecoveryCopy,
  MIN_RECOVERY_PASSWORD_CHARS,
  RECOVERY_COPY_FILE_NAME,
  recordRecoveryCopy,
  recoveryPasswordProblem,
  type MasterKeyStatus,
} from "@/lib/masterKey";

/** What losing the keychain costs, said wherever skipping is offered. */
export const SKIP_WARNING =
  "Without a recovery copy, losing this computer's keychain (reinstalling, moving to a new computer, or resetting your login password) loses every store sign-in and secret this library keeps. Books already downloaded stay.";

/** Props for {@link RecoveryCopyForm}. */
export interface RecoveryCopyFormProps {
  /** The operator saved a copy, or skipped after the warning; the status after the write. */
  onDone: (status: MasterKeyStatus) => void;
  /** Label of the skip button; leave out to offer no skip. */
  skipLabel?: string;
  /** Where the operator can save a copy later, said in the skip warning. */
  laterHint?: string;
  /** Test seam: hands the copy to the browser (defaults to a download). */
  download?: (copy: Blob) => void;
}

/**
 * Saves a recovery copy of the library key: a password and its
 * confirmation, then a download of the copy wrapped with it, then "I've
 * stored it". Skipping, where offered, shows {@link SKIP_WARNING} first.
 */
export function RecoveryCopyForm({
  onDone,
  skipLabel,
  laterHint,
  download = downloadRecoveryCopy,
}: RecoveryCopyFormProps) {
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [downloaded, setDownloaded] = useState(false);
  const [confirmingSkip, setConfirmingSkip] = useState(false);

  async function save() {
    const problem = recoveryPasswordProblem(password, confirm);
    if (problem) {
      setError(problem);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      download(await makeRecoveryCopy(password));
      setDownloaded(true);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not make a recovery copy.");
    } finally {
      setBusy(false);
    }
  }

  async function record(state: "saved" | "skipped") {
    setBusy(true);
    setError(null);
    try {
      const status = await recordRecoveryCopy(state);
      onDone(status);
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not record the answer.");
      setBusy(false);
    }
  }

  const errorLine = error ? (
    <p className="text-sm font-medium text-brick" role="alert">
      {error}
    </p>
  ) : null;

  if (confirmingSkip) {
    return (
      <div className="space-y-4">
        <p
          className="rounded-md border border-brick/30 bg-brick/5 p-3 text-sm text-ink"
          role="note"
        >
          {SKIP_WARNING}
          {laterHint ? ` ${laterHint}` : null}
        </p>
        {errorLine}
        <div className="flex flex-wrap gap-2">
          <Button
            type="button"
            variant="secondary"
            disabled={busy}
            onClick={() => setConfirmingSkip(false)}
          >
            Go back
          </Button>
          <Button type="button" disabled={busy} onClick={() => void record("skipped")}>
            Skip anyway
          </Button>
        </div>
      </div>
    );
  }

  if (downloaded) {
    return (
      <div className="space-y-4">
        <p className="text-sm text-ink/70">
          Your browser saved <strong>{RECOVERY_COPY_FILE_NAME}</strong>, usually in your Downloads
          folder. Move it somewhere off this computer, such as a USB stick or a password manager,
          and keep the password apart from it.
        </p>
        {errorLine}
        <div className="flex flex-wrap gap-2">
          <Button
            type="button"
            variant="secondary"
            disabled={busy}
            onClick={() => setDownloaded(false)}
          >
            Download again
          </Button>
          <Button type="button" disabled={busy} onClick={() => void record("saved")}>
            I've stored it
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="space-y-2">
        <label className="block text-sm font-medium text-ink" htmlFor="recovery-password">
          Recovery password
        </label>
        <Input
          id="recovery-password"
          type="password"
          autoComplete="new-password"
          value={password}
          disabled={busy}
          onChange={(e) => setPassword(e.target.value)}
        />
        <label className="block text-sm font-medium text-ink" htmlFor="recovery-password-confirm">
          Type it again
        </label>
        <Input
          id="recovery-password-confirm"
          type="password"
          autoComplete="new-password"
          value={confirm}
          disabled={busy}
          onChange={(e) => setConfirm(e.target.value)}
        />
        <p className="text-xs text-ink/50">
          At least {MIN_RECOVERY_PASSWORD_CHARS} characters. Without it the copy can't be restored,
          and nobody can reset it.
        </p>
      </div>
      {errorLine}
      <div className="flex flex-wrap gap-2">
        <Button type="button" disabled={busy} onClick={() => void save()}>
          Download recovery copy
        </Button>
        {skipLabel ? (
          <Button
            type="button"
            variant="secondary"
            disabled={busy}
            onClick={() => {
              setError(null);
              setConfirmingSkip(true);
            }}
          >
            {skipLabel}
          </Button>
        ) : null}
      </div>
    </div>
  );
}

/** Props for {@link RecoveryCopyReminder}. */
export interface RecoveryCopyReminderProps {
  /**
   * Whether this session may save a copy (the operator, or an elevated
   * Owner). An Owner who hasn't elevated is told how to.
   */
  canSave: boolean;
  /** Test seam: reads the key's status (defaults to the daemon). */
  load?: () => Promise<MasterKeyStatus>;
  /** Test seam: hands the copy to the browser (defaults to a download). */
  download?: (copy: Blob) => void;
}

/**
 * Settings reminder: shown while the key is in the OS keychain and no
 * recovery copy is recorded as saved, including after the first run's step
 * was skipped. The operator gets it in Server Settings with the form; an
 * Owner gets it under Account with a pointer to elevating. Renders nothing
 * otherwise, or when the status can't be read.
 */
export function RecoveryCopyReminder({
  canSave,
  load = fetchMasterKeyStatus,
  download,
}: RecoveryCopyReminderProps) {
  const [status, setStatus] = useState<MasterKeyStatus | null>(null);
  const [open, setOpen] = useState(false);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    let live = true;
    load()
      .then((s) => {
        if (live) setStatus(s);
      })
      .catch(() => {
        /* no reminder when the status can't be read */
      });
    return () => {
      live = false;
    };
  }, [load]);

  if (saved) {
    return (
      <p className="rounded-md border border-teal/30 bg-teal/5 p-3 text-sm text-ink" role="status">
        Recovery copy saved. Keep it and its password somewhere safe.
      </p>
    );
  }
  if (!status?.needs_recovery_copy) return null;

  return (
    <section
      className="space-y-3 rounded-md border border-brick/30 bg-brick/5 p-4"
      aria-labelledby="recovery-copy-reminder"
    >
      <h2 id="recovery-copy-reminder" className="text-base font-semibold text-ink">
        Save a recovery copy of your library key
      </h2>
      <p className="text-sm text-ink/70">
        The key that protects this library's store sign-ins and secrets is{" "}
        {status.store === "systemd_creds"
          ? "sealed to this computer with systemd-creds"
          : "in this computer's keychain"}
        , so backups of the library folder don't include it.{" "}
        {status.recovery_copy === "skipped"
          ? "You skipped saving a copy when you set up."
          : "No copy has been saved yet."}
      </p>
      {!canSave ? (
        <p className="text-sm text-ink/70">
          To save one, choose <strong>Elevate to Operator</strong> under Account, then open Server
          Settings.
        </p>
      ) : open ? (
        <RecoveryCopyForm
          download={download}
          onDone={(next) => {
            setStatus(next);
            if (!next.needs_recovery_copy) setSaved(true);
          }}
        />
      ) : (
        <Button type="button" onClick={() => setOpen(true)}>
          Save a recovery copy
        </Button>
      )}
    </section>
  );
}

/** Props for {@link RecoveryCopySetupStep}. */
export interface RecoveryCopySetupStepProps {
  /** Carry on with the first run. */
  onDone: () => void;
  /** Test seam: reads the key's status (defaults to the daemon). */
  load?: () => Promise<MasterKeyStatus>;
  /** Test seam: hands the copy to the browser (defaults to a download). */
  download?: (copy: Blob) => void;
}

/**
 * The desktop first run's step after starting a library: save a recovery
 * copy of the key the window just moved into the OS keychain, or skip after
 * a warning. Carries straight on when there is nothing to ask (the key
 * stayed in a file, a copy is already saved, or the status can't be read).
 */
export function RecoveryCopySetupStep({
  onDone,
  load = fetchMasterKeyStatus,
  download,
}: RecoveryCopySetupStepProps) {
  const [asking, setAsking] = useState(false);
  // The parent's continuation, read when the status arrives; a new
  // function from a parent render doesn't reload the status.
  const done = useRef(onDone);
  useEffect(() => {
    done.current = onDone;
  }, [onDone]);

  useEffect(() => {
    let live = true;
    load()
      .then((s) => {
        if (!live) return;
        if (s.needs_recovery_copy) setAsking(true);
        else done.current();
      })
      .catch(() => {
        // Settings reminds them; don't block the first run on it.
        if (live) done.current();
      });
    return () => {
      live = false;
    };
  }, [load]);

  if (!asking) return <p className="text-sm text-ink/50">Checking the library key…</p>;
  return (
    <RecoveryCopyForm
      download={download}
      skipLabel="Skip for now"
      laterHint="You can save one later under Settings → Server Settings."
      onDone={() => onDone()}
    />
  );
}
