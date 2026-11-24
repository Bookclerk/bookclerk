import { useCallback, useEffect, useRef, useState } from "react";
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

/** How often the panel re-reads the status while a test is on its way. */
const TEST_POLL_MS = 3_000;

/** How long it keeps re-reading before it stops waiting for the test. */
const TEST_POLL_LIMIT_MS = 60_000;

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
  // The `tested_at` a sent test must replace, while one is on its way.
  const [waitingPast, setWaitingPast] = useState<string | null | undefined>(undefined);
  const mounted = useRef(true);
  // Advanced by each saved notifier choice; a status read that started
  // before a save is discarded so it cannot show the old choice.
  const selection = useRef(0);

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
    mounted.current = true;
    void load();
    return () => {
      mounted.current = false;
    };
  }, [load]);

  // After a test is queued, re-read the status until its delivery shows as
  // a new `tested_at`, or stop at a deadline that does not wait on a
  // request: a read still in flight then is ignored when it settles.
  useEffect(() => {
    if (waitingPast === undefined) return;
    let done = false;
    let inFlight = false;
    const timer = window.setInterval(() => {
      if (inFlight) return;
      inFlight = true;
      void (async () => {
        try {
          const generation = selection.current;
          const next = await getAccountMail();
          if (done || !mounted.current || generation !== selection.current) return;
          setStatus(next);
          if (next.tested_at && next.tested_at !== waitingPast) {
            done = true;
            setWaitingPast(undefined);
            setNotice("The test arrived.");
          }
        } catch {
          // A failed read is retried on the next tick.
        } finally {
          inFlight = false;
        }
      })();
    }, TEST_POLL_MS);
    const deadline = window.setTimeout(() => {
      if (done || !mounted.current) return;
      done = true;
      setWaitingPast(undefined);
      setNotice("The test has not arrived yet. Check the notifier, then try again.");
    }, TEST_POLL_LIMIT_MS);
    return () => {
      done = true;
      window.clearInterval(timer);
      window.clearTimeout(deadline);
    };
  }, [waitingPast]);

  async function choose(id: string) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const next = await putAccountMail(id === "" ? null : id);
      selection.current += 1;
      setStatus(next);
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
      setWaitingPast(status?.tested_at ?? null);
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
          disabled={
            busy || waitingPast !== undefined || !chosen || status?.not_ready === "notifier_missing"
          }
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
