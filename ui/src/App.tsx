import { useEffect, useState } from "react";
import { AccountsPage } from "@/components/AccountsPage";
import { DiscoverPage } from "@/components/DiscoverPage";
import { LibraryPage } from "@/components/LibraryPage";
import { LoginPage } from "@/components/LoginPage";
import { WishlistPage } from "@/components/WishlistPage";
import { authMe, type AppView, type AuthSession } from "@/lib/api";

type AuthState = "loading" | "anon" | "authed";

function normalizeView(v: string | undefined): AppView {
  if (v === "library" || v === "accounts" || v === "discover" || v === "wishlist") {
    return v;
  }
  return "discover";
}

export default function App() {
  const [auth, setAuth] = useState<AuthState>("loading");
  const [session, setSession] = useState<AuthSession | null>(null);
  const [view, setView] = useState<AppView>("discover");

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const me = await authMe();
        if (cancelled) return;
        if (me.authenticated) {
          setSession(me);
          setView(normalizeView(me.default_view));
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
  }, []);

  function onLoginSuccess(next: AuthSession) {
    setSession(next);
    setView(normalizeView(next.default_view));
    setAuth("authed");
  }

  function onLogout() {
    setSession(null);
    setAuth("anon");
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

  const nav = { view, onNavigate: setView };
  const canAcquire = session.can_acquire;
  const role = session.role;

  if (view === "accounts") {
    return (
      <AccountsPage onLogout={onLogout} nav={nav} role={role} />
    );
  }

  if (view === "wishlist") {
    return <WishlistPage onLogout={onLogout} nav={nav} role={role} />;
  }

  if (view === "discover") {
    return (
      <DiscoverPage
        onLogout={onLogout}
        nav={nav}
        role={role}
        defaultView={session.default_view}
        onDefaultViewChange={(v) =>
          setSession((s) => (s ? { ...s, default_view: v } : s))
        }
      />
    );
  }

  return (
    <LibraryPage
      onLogout={onLogout}
      canAcquire={canAcquire}
      nav={nav}
      role={role}
    />
  );
}
