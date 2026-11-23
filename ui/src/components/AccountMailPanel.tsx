import { useCallback, useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  getAccountMail,
  isApiError,
  listNotifiers,
  putAccountMail,
  sendAccountMailTest,
  type AccountMailNotReady,
  type AccountMailStatus,
  type NotifierOption,
} from "@/lib/api";

/** What each reason account mail is off means, in the panel's words. */
const NOT_READY: Record<AccountMailNotReady, string> = {
  not_configured: "Off. Choose a notifier to mail invites and verification links.",
  notifier_missing: "The chosen notifier is not installed any more. Choose another.",
  no_public_origin:
    "Set the library's public address first; every link in account mail is built from it.",
};

/**
 * Settings → User Management → Account mail: which notifier sends invites
 * and verification links, whether it is ready, and a test to the
 * signed-in administrator's own address.
 */
export function AccountMailPanel() {
  const [status, setStatus] = useState<AccountMailStatus | null>(null);
  const [notifiers, setNotifiers] = useState<NotifierOption[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [next, all] = await Promise.all([getAccountMail(), listNotifiers()]);
      setStatus(next);
      setNotifiers(all.filter((n) => n.account_mail));
    } catch (err) {
      setError(isApiError(err) ? err.message : "Could not read account mail settings");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  async function choose(id: string) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      setStatus(await putAccountMail(id === "" ? null : id));
    } catch (err) {
      setError(isApiError(err) ? err.message : "Could not save");
    } finally {
      setBusy(false);
    }
  }

  async function test() {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await sendAccountMailTest();
      setNotice("Test sent to your address. This page shows when it arrives.");
    } catch (err) {
      setError(isApiError(err) ? err.message : "Could not send a test");
    } finally {
      setBusy(false);
    }
  }

  const chosen = status?.plugin_instance_id ?? "";
  const testedAt = status?.tested_at ? new Date(status.tested_at).toLocaleString() : null;

  return (
    <section className="space-y-3" aria-labelledby="account-mail-heading">
      <div className="space-y-1">
        <h2 id="account-mail-heading" className="text-lg font-semibold text-ink">
          Account mail
        </h2>
        <p className="text-sm text-ink/70">
          Invites and links that verify a patron's email address go through this notifier.
        </p>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <label className="text-sm text-ink" htmlFor="account-mail-notifier">
          Notifier
        </label>
        <select
          id="account-mail-notifier"
          className="rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm text-ink shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
          value={chosen}
          disabled={busy || status === null}
          onChange={(e) => void choose(e.target.value)}
        >
          <option value="">Off</option>
          {notifiers.map((n) => (
            <option key={n.plugin_instance_id} value={n.plugin_instance_id}>
              {n.name}
            </option>
          ))}
          {chosen && !notifiers.some((n) => n.plugin_instance_id === chosen) ? (
            <option value={chosen}>Missing notifier</option>
          ) : null}
        </select>
        <Button
          type="button"
          variant="secondary"
          disabled={busy || !chosen || status?.not_ready === "notifier_missing"}
          onClick={() => void test()}
        >
          Send a test
        </Button>
      </div>
      {status ? (
        <p className="text-sm text-ink/70" role="status">
          {status.ready ? "Ready." : NOT_READY[status.not_ready ?? "not_configured"]}
          {testedAt ? ` Last test arrived ${testedAt}.` : chosen ? " No test has arrived yet." : ""}
        </p>
      ) : null}
      {notifiers.length === 0 && status ? (
        <p className="text-sm text-ink/70">
          Install a notifier that can send account mail, such as the email plugin, to turn this on.
        </p>
      ) : null}
      {notice ? <p className="text-sm text-ink/80">{notice}</p> : null}
      {error ? (
        <p className="text-sm font-medium text-brick-text" role="alert">
          {error}
        </p>
      ) : null}
    </section>
  );
}
