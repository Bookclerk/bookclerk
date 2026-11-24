import { useEffect, useRef, useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { Button } from "@/components/ui/button";
import { isApiError, verifyEmail } from "@/lib/api";

type Phase =
  | { kind: "working" }
  | { kind: "verified" }
  /** `retry`: the request got no answer, so the same link may be sent again. */
  | { kind: "failed"; message: string; retry: boolean };

/**
 * `/verify-email?token=…`: uses a mailed link that verifies a patron's
 * email address. The link is the proof, so the page works without a
 * sign-in. The token is removed from the address bar before the request,
 * so it is not kept in history.
 */
export function VerifyEmailPage() {
  const [phase, setPhase] = useState<Phase>({ kind: "working" });
  // One request per page load (StrictMode runs effects twice).
  const sent = useRef(false);
  // The token, kept in memory only for Try again.
  const token = useRef<string | null>(null);

  function send(value: string) {
    setPhase({ kind: "working" });
    void verifyEmail(value)
      .then(() => setPhase({ kind: "verified" }))
      .catch((err: unknown) => {
        setPhase(
          isApiError(err) && err.status === 400
            ? {
                kind: "failed",
                message:
                  "This link is invalid or has expired. Ask for a new one from your profile in Settings → Account.",
                retry: false,
              }
            : {
                kind: "failed",
                message: "The library did not answer. Check your connection and try again.",
                retry: true,
              },
        );
      });
  }

  useEffect(() => {
    if (sent.current) return;
    sent.current = true;
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
    send(value);
  }, []);

  const toApp = (
    <Button
      type="button"
      variant={phase.kind === "failed" && phase.retry ? "secondary" : "primary"}
      onClick={() => window.location.assign("/")}
    >
      Open Bookclerk
    </Button>
  );

  if (phase.kind === "working") {
    return (
      <JoinShell title="Verifying your email…" lead="Checking the link with the library.">
        <p className="text-sm text-ink/70">One moment.</p>
      </JoinShell>
    );
  }

  if (phase.kind === "failed") {
    return (
      <JoinShell title="Could not verify your email" lead={phase.message}>
        <div className="flex gap-2">
          {phase.retry && token.current ? (
            <Button
              type="button"
              onClick={() => {
                if (token.current) send(token.current);
              }}
            >
              Try again
            </Button>
          ) : null}
          {toApp}
        </div>
      </JoinShell>
    );
  }

  return (
    <JoinShell
      title="Email verified"
      lead="Your address is confirmed. The library can now send you account mail, such as password resets."
    >
      <div className="flex gap-2">{toApp}</div>
    </JoinShell>
  );
}
