import { useEffect, useState, type FormEvent } from "react";
import { Link2, RefreshCw, Unlink } from "lucide-react";
import type { AppNavProps } from "@/components/AppNav";
import { AppTopBar } from "@/components/AppTopBar";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  portalConnections,
  portalRevokeConnection,
  portalSourceLogin,
  portalSourceOauthStart,
  portalSources,
  signOut,
  type AuthRole,
  type PortalConnection,
  type PortalSource,
} from "@/lib/api";
import { cn, pageWidthClass } from "@/lib/utils";

/**
 * Storefront Accounts page — connect and revoke linked sources.
 *
 * @param props - Logout handler, nav props, and optional session role.
 */
export function AccountsPage({
  onLogout,
  nav,
  role,
}: {
  onLogout: () => void;
  nav: AppNavProps;
  role?: AuthRole;
}) {
  const [sources, setSources] = useState<PortalSource[]>([]);
  const [connections, setConnections] = useState<PortalConnection[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [openSource, setOpenSource] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [oauthUrl, setOauthUrl] = useState<string | null>(null);
  const canConnect = role === "administrator" || role === "member";

  async function refresh() {
    setError(null);
    setBusy(true);
    try {
      const src = await portalSources();
      setSources(src);
      try {
        const conns = await portalConnections();
        setConnections(conns);
      } catch (err) {
        setConnections([]);
        if (!canConnect) {
          setError(
            "Store linking requires a user session. Sign in with a claim ticket or integration account (operators cannot connect stores).",
          );
        } else {
          throw err;
        }
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to load accounts");
    } finally {
      setBusy(false);
    }
  }

  useEffect(() => {
    void refresh();
  }, []);

  async function onPasswordLogin(e: FormEvent, sourceId: string) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await portalSourceLogin(sourceId, {
        email: email.trim(),
        password,
      });
      setEmail("");
      setPassword("");
      setOpenSource(null);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Login failed");
      setBusy(false);
    }
  }

  async function onOauthStart(sourceId: string) {
    setBusy(true);
    setError(null);
    setOauthUrl(null);
    try {
      const res = await portalSourceOauthStart(sourceId);
      setOauthUrl(res.url);
      setOpenSource(sourceId);
      window.open(res.url, "_blank", "noopener");
    } catch (err) {
      setError(err instanceof Error ? err.message : "OAuth start failed");
    } finally {
      setBusy(false);
    }
  }

  async function onRevoke(accountId: string) {
    if (!window.confirm("Revoke store credentials? Acquired books are kept.")) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await portalRevokeConnection(accountId);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Revoke failed");
      setBusy(false);
    }
  }

  async function onSignOut() {
    await signOut(role);
    onLogout();
  }

  return (
    <div className="flex h-full flex-col">
      <header className="sticky top-0 z-10 border-b border-ink/10 bg-paper/85 px-3 py-3 backdrop-blur-md sm:px-5">
        <div className={pageWidthClass}>
          <AppTopBar
            nav={nav}
            onSignOut={onSignOut}
            actions={
              <Button
                variant="secondary"
                onClick={() => void refresh()}
                disabled={busy}
              >
                <RefreshCw className="h-4 w-4" />
                Refresh
              </Button>
            }
          />
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
      <main className={cn("flex w-full flex-col gap-8 px-4 py-6 sm:px-5", pageWidthClass)}>
        <div className="space-y-1">
          <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">
            Accounts
          </h1>
          <p className="text-sm text-ink/60">
            Link bookstore accounts. Acquired books stay when you revoke.
          </p>
        </div>

        {error ? (
          <p className="text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}

        <section className="space-y-3">
          <h2 className="text-lg font-semibold text-ink">Bookstore sources</h2>
          {sources.length === 0 ? (
            <p className="text-sm text-ink/50">No sources enabled.</p>
          ) : (
            <ul className="divide-y divide-ink/10 bg-white/35">
              {sources.map((s) => {
                const open = openSource === s.id;
                return (
                  <li key={s.id} className="px-3 py-3">
                    <div className="flex flex-wrap items-center justify-between gap-2">
                      <div className="flex items-center gap-3">
                        {s.brand.logo ? (
                          <img
                            src={s.brand.logo}
                            alt=""
                            className="h-8 w-8 rounded bg-white object-contain p-0.5"
                          />
                        ) : (
                          <div className="flex h-8 w-8 items-center justify-center rounded bg-fold text-xs font-semibold">
                            {s.name.slice(0, 2)}
                          </div>
                        )}
                        <div>
                          <p className="font-medium text-ink">{s.name}</p>
                          <p className="text-xs text-ink/50">
                            {s.auth === "oauth" ? "OAuth" : "Email & password"} · {s.id}
                          </p>
                        </div>
                      </div>
                      <Button
                        variant="secondary"
                        disabled={busy || !canConnect}
                        onClick={() => {
                          if (s.auth === "oauth") {
                            void onOauthStart(s.id);
                          } else {
                            setOauthUrl(null);
                            setOpenSource(open ? null : s.id);
                          }
                        }}
                      >
                        <Link2 className="h-4 w-4" />
                        Connect
                      </Button>
                    </div>
                    {open && s.auth === "password" ? (
                      <form
                        className="mt-3 grid gap-2 sm:grid-cols-[1fr_1fr_auto]"
                        onSubmit={(e) => void onPasswordLogin(e, s.id)}
                      >
                        <Input
                          type="email"
                          placeholder="Email"
                          value={email}
                          onChange={(e) => setEmail(e.target.value)}
                          autoComplete="username"
                          required
                        />
                        <Input
                          type="password"
                          placeholder="Password"
                          value={password}
                          onChange={(e) => setPassword(e.target.value)}
                          autoComplete="current-password"
                          required
                        />
                        <Button type="submit" disabled={busy}>
                          Save login
                        </Button>
                      </form>
                    ) : null}
                    {open && s.auth === "oauth" && oauthUrl ? (
                      <p className="mt-2 text-sm text-ink/60">
                        Complete sign-in in the new tab, or{" "}
                        <a
                          href={oauthUrl}
                          target="_blank"
                          rel="noreferrer"
                          className="text-teal underline"
                        >
                          open {s.name} login
                        </a>
                        .
                      </p>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <section className="space-y-3">
          <h2 className="text-lg font-semibold text-ink">Connections</h2>
          {connections.length === 0 ? (
            <p className="text-sm text-ink/50">No store connections yet.</p>
          ) : (
            <ul className="divide-y divide-ink/10 bg-white/35">
              {connections.map((c) => (
                <li
                  key={c.account_id}
                  className="flex flex-wrap items-center justify-between gap-2 px-3 py-3"
                >
                  <div className="flex items-center gap-3">
                    {c.brand?.logo ? (
                      <img
                        src={c.brand.logo}
                        alt=""
                        className="h-7 w-7 rounded bg-white object-contain p-0.5"
                      />
                    ) : null}
                    <div>
                      <p className="font-medium text-ink">
                        {c.label || c.account_id}
                      </p>
                      <p className="text-xs text-ink/50">
                        {c.source} · {c.connection_status}
                        {!c.source_enabled ? " · source disabled" : ""}
                      </p>
                    </div>
                  </div>
                  {c.connection_status !== "revoked" ? (
                    <Button
                      variant="ghost"
                      disabled={busy}
                      onClick={() => void onRevoke(c.account_id)}
                    >
                      <Unlink className="h-4 w-4" />
                      Revoke
                    </Button>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </section>
      </main>
      </div>
    </div>
  );
}
