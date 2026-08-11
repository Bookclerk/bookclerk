import { useEffect, useState } from "react";
import { AccountsPage } from "@/components/AccountsPage";
import { DiscoverPage } from "@/components/DiscoverPage";
import { LibraryPage } from "@/components/LibraryPage";
import { LoginPage } from "@/components/LoginPage";
import { NotFoundPage } from "@/components/NotFoundPage";
import { PreferencesProvider } from "@/components/PreferencesDialog";
import { SettingsPage } from "@/components/SettingsPage";
import { WishlistPage } from "@/components/WishlistPage";
import { Button } from "@/components/ui/button";
import {
  authMe,
  stopImpersonate,
  type AppView,
  type AuthSession,
} from "@/lib/api";
import {
  isAppPath,
  normalizeAppView,
  resolveView,
  syncUrlToView,
  viewFromPath,
} from "@/lib/routes";

type AuthState = "loading" | "anon" | "authed";

export default function App() {
  const [auth, setAuth] = useState<AuthState>("loading");
  const [session, setSession] = useState<AuthSession | null>(null);
  const [view, setView] = useState<AppView>(
    () => viewFromPath(window.location.pathname) ?? "discover",
  );
  const knownPath = isAppPath(window.location.pathname);

  useEffect(() => {
    if (!knownPath) return;
    let cancelled = false;
    void (async () => {
      try {
        const me = await authMe();
        if (cancelled) return;
        if (me.authenticated) {
          const next = resolveView(window.location.pathname, me.default_view);
          setSession(me);
          setView(next);
          syncUrlToView(next, "replace");
          setAuth("authed");
        } else {
          setAuth("anon");
        }
      } catch {
        if (!cancelled) setAuth("anon");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [knownPath]);

  useEffect(() => {
    if (!knownPath || auth !== "authed") return;
    const onPopState = () => {
      const pathView = viewFromPath(window.location.pathname);
      if (pathView) {
        setView(pathView);
        return;
      }
      // `/` or other app root → fall back to the signed-in default.
      const fallback = normalizeAppView(session?.default_view);
      setView(fallback);
      syncUrlToView(fallback, "replace");
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [knownPath, auth, session?.default_view]);

  function navigate(next: AppView) {
    setView(next);
    syncUrlToView(next, "push");
  }

  function onLoginSuccess(next: AuthSession) {
    const resolved = resolveView(window.location.pathname, next.default_view);
    setSession(next);
    setView(resolved);
    syncUrlToView(resolved, "replace");
    setAuth("authed");
  }

  function onLogout() {
    setSession(null);
    setAuth("anon");
  }

  async function onStopImpersonate() {
    try {
      await stopImpersonate();
      const me = await authMe();
      setSession(me);
    } catch {
      /* keep banner; next navigation will refresh */
    }
  }

  async function refreshSession() {
    const me = await authMe();
    setSession(me);
  }

  // Unknown paths stay a branded 404 (API/static assets are not SPA routes).
  if (!knownPath) {
    return <NotFoundPage />;
  }

  if (auth === "loading") {
    return (
      <div className="flex min-h-full items-center justify-center text-sm text-ink/60">
        Loading Bookclerk…
      </div>
    );
  }

  if (auth === "anon" || !session) {
    return <LoginPage onSuccess={onLoginSuccess} />;
  }

  const nav = { view, onNavigate: navigate };
  const canAcquire = session.can_acquire;
  const role = session.role;

  let page = (
    <LibraryPage
      onLogout={onLogout}
      canAcquire={canAcquire}
      nav={nav}
      role={role}
    />
  );

  if (view === "accounts") {
    page = <AccountsPage onLogout={onLogout} nav={nav} role={role} />;
  } else if (view === "wishlist") {
    page = <WishlistPage onLogout={onLogout} nav={nav} role={role} />;
  } else if (view === "settings") {
    page = (
      <SettingsPage
        onLogout={onLogout}
        onSessionExpired={onLogout}
        onSessionChange={refreshSession}
        nav={nav}
        role={role}
        session={session}
      />
    );
  } else if (view === "discover") {
    page = <DiscoverPage onLogout={onLogout} nav={nav} role={role} />;
  }

  return (
    <PreferencesProvider
      defaultView={session.default_view}
      onDefaultViewChange={(v) =>
        setSession((s) => (s ? { ...s, default_view: v } : s))
      }
    >
      {session.impersonating ? (
        <div className="flex items-center justify-between gap-3 bg-brick px-4 py-2 text-sm text-white">
          <span>
            Impersonating{" "}
            <strong>
              {session.impersonating.display_name?.trim() ||
                `user #${session.impersonating.user_id}`}
            </strong>
          </span>
          <Button
            type="button"
            variant="secondary"
            className="h-8 bg-white text-brick hover:bg-white/90"
            onClick={() => void onStopImpersonate()}
          >
            Stop
          </Button>
        </div>
      ) : null}
      {page}
    </PreferencesProvider>
  );
}
