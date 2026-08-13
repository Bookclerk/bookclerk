import { useEffect, useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  authMe,
  login,
  passwordLogin,
  portalLoginIntegration,
  portalRedeem,
  type AuthSession,
} from "@/lib/api";
import { cn } from "@/lib/utils";

type Tab = "operator" | "password" | "claim" | "return";

/** Pull `#token=…` / `#operator_token=…` from the tray's open-UI URL. */
function tokenFromHash(): string | null {
  const raw = window.location.hash.replace(/^#/, "");
  if (!raw) return null;
  const params = new URLSearchParams(raw);
  const token = params.get("token") ?? params.get("operator_token");
  return token?.trim() || null;
}

function clearHash() {
  const { pathname, search } = window.location;
  window.history.replaceState(null, "", `${pathname}${search}`);
}

/**
 * Unauthenticated entry — operator token, password, claim ticket, or return login.
 *
 * @param props - Called with the new session after successful sign-in.
 */
export function LoginPage({
  onSuccess,
}: {
  onSuccess: (session: AuthSession) => void;
}) {
  const [tab, setTab] = useState<Tab>(() => {
    const params = new URLSearchParams(window.location.search);
    return params.get("ticket") ? "claim" : "operator";
  });
  const [token, setToken] = useState("");
  const [ticket, setTicket] = useState(() => {
    const params = new URLSearchParams(window.location.search);
    return params.get("ticket") ?? "";
  });
  const [claimPassword, setClaimPassword] = useState("");
  const [claimPasswordConfirm, setClaimPasswordConfirm] = useState("");
  const [provider, setProvider] = useState("audiobookshelf");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const t = params.get("ticket");
    if (t) {
      setTicket(t);
      setTab("claim");
    }
  }, []);

  // Tray "Open Bookclerk" links carry the operator token in the fragment.
  useEffect(() => {
    const fromHash = tokenFromHash();
    if (!fromHash) return;
    clearHash();
    setTab("operator");
    setToken(fromHash);
    setBusy(true);
    setError(null);
    let cancelled = false;
    void (async () => {
      try {
        const session = await login(fromHash);
        if (!cancelled) onSuccess(session);
      } catch (err) {
        if (!cancelled) {
          setError(
            err instanceof Error ? err.message : "Invalid operator token.",
          );
        }
      } finally {
        if (!cancelled) setBusy(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [onSuccess]);

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
      // Surface server text (e.g. the login-throttle 429 retry hint).
      setError(err instanceof Error ? err.message : "Invalid operator token.");
    } finally {
      setBusy(false);
    }
  }

  async function onClaimSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const password = claimPassword.trim();
    const confirm = claimPasswordConfirm.trim();
    if (password && password !== confirm) {
      setError("Passwords do not match.");
      setBusy(false);
      return;
    }
    try {
      await portalRedeem(ticket.trim(), password || undefined);
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid or expired ticket.");
    } finally {
      setBusy(false);
    }
  }

  async function onReturnSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await portalLoginIntegration({
        provider: provider.trim(),
        username: username.trim(),
        password,
      });
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Sign-in failed.");
    } finally {
      setBusy(false);
    }
  }

  async function onPasswordSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await passwordLogin(username.trim(), password);
      await finishPortal();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Invalid login or password.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <div className="w-full max-w-md animate-[fadeUp_420ms_ease-out]">
        <img
          src="/bookclerk-logo.svg"
          alt="Bookclerk"
          className="mb-8 h-12 w-auto"
        />
        <h1 className="font-display text-3xl font-bold tracking-tight text-ink">
          Sign in
        </h1>
        <p className="mt-2 text-sm text-ink/70">
          Operator token, local password, claim ticket, or integration return.
        </p>

        <div
          className="mt-6 flex flex-wrap gap-1 rounded-md border border-ink/10 bg-white/40 p-1"
          role="tablist"
        >
          {(
            [
              ["operator", "Operator"],
              ["password", "Password"],
              ["claim", "Claim"],
              ["return", "Return"],
            ] as const
          ).map(([id, label]) => (
            <button
              key={id}
              type="button"
              role="tab"
              aria-selected={tab === id}
              className={cn(
                "flex-1 rounded px-2 py-1.5 text-sm font-medium transition-colors",
                tab === id
                  ? "bg-ink text-paper shadow-sm"
                  : "text-ink/60 hover:text-ink",
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

        {tab === "operator" ? (
          <form onSubmit={onOperatorSubmit} className="mt-5">
            <p className="text-sm text-ink/70">
              Paste the operator API token from{" "}
              <code className="rounded bg-fold/60 px-1 py-0.5 text-[13px]">
                operator.token
              </code>{" "}
              (or{" "}
              <code className="rounded bg-fold/60 px-1 py-0.5 text-[13px]">
                BOOKCLERK_OPERATOR_TOKEN
              </code>
              ).
            </p>
            <label className="mt-4 block text-sm font-semibold" htmlFor="token">
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
            {error ? (
              <p className="mt-2 text-sm font-medium text-brick" role="alert">
                {error}
              </p>
            ) : null}
            <Button type="submit" className="mt-5 w-full" disabled={busy || !token}>
              {busy ? "Signing in…" : "Open library"}
            </Button>
          </form>
        ) : null}

        {tab === "password" ? (
          <form onSubmit={onPasswordSubmit} className="mt-5">
            <p className="text-sm text-ink/70">
              Sign in with a local Bookclerk username and password.
            </p>
            <label className="mt-4 block text-sm font-semibold" htmlFor="local-login">
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
            {error ? (
              <p className="mt-2 text-sm font-medium text-brick" role="alert">
                {error}
              </p>
            ) : null}
            <Button
              type="submit"
              className="mt-5 w-full"
              disabled={busy || !username || !password}
            >
              {busy ? "Signing in…" : "Sign in"}
            </Button>
          </form>
        ) : null}

        {tab === "claim" ? (
          <form onSubmit={onClaimSubmit} className="mt-5">
            <p className="text-sm text-ink/70">
              Use an invite magic link or paste the ticket below. Set a password when
              required — for example on invite or password-reset tickets when the account has
              no password yet.
            </p>
            <label className="mt-4 block text-sm font-semibold" htmlFor="ticket">
              Claim ticket
            </label>
            <Input
              id="ticket"
              value={ticket}
              onChange={(e) => setTicket(e.target.value)}
              className="mt-1.5"
              placeholder="Paste ticket"
              autoComplete="off"
              spellCheck={false}
              required
            />
            <label className="mt-4 block text-sm font-semibold" htmlFor="claim-password">
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
            <label className="mt-4 block text-sm font-semibold" htmlFor="claim-password-confirm">
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
            {error ? (
              <p className="mt-2 text-sm font-medium text-brick" role="alert">
                {error}
              </p>
            ) : null}
            <Button
              type="submit"
              className="mt-5 w-full"
              disabled={busy || !ticket.trim()}
            >
              {busy ? "Redeeming…" : "Continue"}
            </Button>
          </form>
        ) : null}

        {tab === "return" ? (
          <form onSubmit={onReturnSubmit} className="mt-5">
            <p className="text-sm text-ink/70">
              Sign in with your integration credentials to manage store links.
            </p>
            <label className="mt-4 block text-sm font-semibold" htmlFor="provider">
              Provider
            </label>
            <Input
              id="provider"
              value={provider}
              onChange={(e) => setProvider(e.target.value)}
              className="mt-1.5"
              placeholder="audiobookshelf"
              required
            />
            <label className="mt-3 block text-sm font-semibold" htmlFor="username">
              Username
            </label>
            <Input
              id="username"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              className="mt-1.5"
              autoComplete="username"
              required
            />
            <label className="mt-3 block text-sm font-semibold" htmlFor="password">
              Password
            </label>
            <Input
              id="password"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className="mt-1.5"
              autoComplete="current-password"
              required
            />
            {error ? (
              <p className="mt-2 text-sm font-medium text-brick" role="alert">
                {error}
              </p>
            ) : null}
            <Button
              type="submit"
              className="mt-5 w-full"
              disabled={busy || !provider.trim() || !username.trim() || !password}
            >
              {busy ? "Signing in…" : "Sign in"}
            </Button>
          </form>
        ) : null}
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
