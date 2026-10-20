import { lazy, Suspense, useEffect, useState, type ReactNode } from "react";
import { LoginPage } from "@/components/LoginPage";
import { NotFoundPage } from "@/components/NotFoundPage";
import { PreferencesProvider } from "@/components/PreferencesDialog";
import { useTheme } from "@/components/ThemeProvider";
import { Button } from "@/components/ui/button";
import {
  authMe,
  endElevate,
  fetchPreferences,
  logout,
  stopImpersonate,
  type AppView,
  type AuthRole,
  type AuthSession,
} from "@/lib/api";
import {
  dismissWelcome,
  fetchJoinStatus,
  rememberApproveReturn,
  takeApproveReturn,
  welcomeDismissed,
} from "@/lib/join";
import {
  dropTicketUnlessInvitePath,
  isAppPath,
  isInvitePath,
  joinPage,
  normalizeAppView,
  resolveView,
  syncUrlToView,
  viewFromPath,
} from "@/lib/routes";
import { showStatusNav } from "@/lib/nav";
import type { SettingsTab } from "@/components/SettingsPage";

const AccountsPage = lazy(() =>
  import("@/components/AccountsPage").then((m) => ({ default: m.AccountsPage })),
);
const DiscoverPage = lazy(() =>
  import("@/components/DiscoverPage").then((m) => ({ default: m.DiscoverPage })),
);
const LibraryPage = lazy(() =>
  import("@/components/LibraryPage").then((m) => ({ default: m.LibraryPage })),
);
const MfaEnrollDialog = lazy(() =>
  import("@/components/MfaEnrollDialog").then((m) => ({
    default: m.MfaEnrollDialog,
  })),
);
const SettingsPage = lazy(() =>
  import("@/components/SettingsPage").then((m) => ({ default: m.SettingsPage })),
);
const StatusPage = lazy(() =>
  import("@/components/StatusPage").then((m) => ({ default: m.StatusPage })),
);
const WishlistPage = lazy(() =>
  import("@/components/WishlistPage").then((m) => ({ default: m.WishlistPage })),
);
const JoinApprovePage = lazy(() =>
  import("@/components/JoinApprovePage").then((m) => ({ default: m.JoinApprovePage })),
);
const JoinCallbackPage = lazy(() =>
  import("@/components/JoinCallbackPage").then((m) => ({ default: m.JoinCallbackPage })),
);
const WelcomeScreen = lazy(() =>
  import("@/components/WelcomeScreen").then((m) => ({ default: m.WelcomeScreen })),
);

/**
 * Before the session check: an SSO sign-in started on the consent page
 * comes back to `/`; send it back to the consent page.
 */
function restoreApproveReturn(): void {
  if (window.location.pathname !== "/") return;
  const back = takeApproveReturn();
  if (back) window.history.replaceState(null, "", back);
}

type AuthState = "loading" | "anon" | "authed";

/**
 * Full-viewport placeholder while a lazy route chunk loads.
 */
function AppLoading() {
  return (
    <div className="flex min-h-full items-center justify-center text-sm text-ink/60">
      Loading Bookclerk…
    </div>
  );
}

function PageSuspense({ children }: { children: ReactNode }) {
  return <Suspense fallback={<AppLoading />}>{children}</Suspense>;
}

/**
 * True when host policy requires a passkey or TOTP and this user has neither.
 */
function needsSecondFactorEnrollment(session: AuthSession | null): boolean {
  return Boolean(
    session?.user && session.second_factor?.required && !session.second_factor.enrolled,
  );
}

function formatAuthRole(role?: AuthRole | string): string {
  switch (role) {
    case "owner":
      return "Owner";
    case "administrator":
      return "Administrator";
    case "host":
      return "Host";
    case "member":
      return "Member";
    case "operator":
      return "Operator";
    default:
      return role?.trim() || "";
  }
}

/**
 * Root SPA shell — auth gate, URL sync, and view routing.
 */
export default function App() {
  const [auth, setAuth] = useState<AuthState>("loading");
  const [session, setSession] = useState<AuthSession | null>(null);
  const { setPreference: setThemePreference } = useTheme();
  // Join a library pages own their URL until they hand over to the app.
  const [join, setJoin] = useState(() => {
    restoreApproveReturn();
    return joinPage(window.location.pathname);
  });
  const [welcome, setWelcome] = useState<{ defaultBranchName: string } | null>(null);
  const [settingsTab, setSettingsTab] = useState<SettingsTab | undefined>();
  const [view, setView] = useState<AppView>(
    () => viewFromPath(window.location.pathname) ?? "discover",
  );
  const knownPath = isAppPath(window.location.pathname);

  useEffect(() => {
    if (!knownPath) return;
    dropTicketUnlessInvitePath();
    let cancelled = false;
    void (async () => {
      try {
        const me = await authMe();
        if (cancelled) return;
        if (me.authenticated && joinPage() != null) {
          if (joinPage() === "approve") takeApproveReturn();
          setSession(me);
          setAuth("authed");
        } else if (me.authenticated) {
          const fromInvite = isInvitePath();
          const next = resolveView(
            fromInvite ? "/" : window.location.pathname,
            me.default_view,
          );
          setSession(me);
          setView(next);
          if (fromInvite) {
            window.history.replaceState(null, "", "/");
          } else {
            syncUrlToView(next, "replace");
          }
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
    if (auth !== "authed") return;
    let cancelled = false;
    void fetchPreferences()
      .then((prefs) => {
        if (!cancelled) setThemePreference(prefs.theme);
      })
      .catch(() => {
        /* keep local / FOUC theme */
      });
    return () => {
      cancelled = true;
    };
  }, [auth, setThemePreference]);

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

  // Back/forward and other view changes leave Settings on its default
  // section next time.
  useEffect(() => {
    if (view !== "settings") setSettingsTab(undefined);
  }, [view]);

  function navigate(next: AppView, tab?: SettingsTab) {
    // A requested Settings section applies only to the navigation that
    // asked for it; Settings otherwise opens on its default section.
    setSettingsTab(next === "settings" ? tab : undefined);
    setView(next);
    syncUrlToView(next, "push");
  }

  // A fresh install's operator first chooses: start a new library, or join
  // one (the welcome screen).
  useEffect(() => {
    if (auth !== "authed" || join != null) return;
    if (session?.role !== "operator" || session.impersonating || welcomeDismissed()) return;
    let cancelled = false;
    void fetchJoinStatus()
      .then((status) => {
        if (!cancelled && status.fresh) {
          setWelcome({ defaultBranchName: status.default_branch_name });
        }
      })
      .catch(() => {
        /* no welcome screen: the app works as before */
      });
    return () => {
      cancelled = true;
    };
  }, [auth, join, session?.role, session?.impersonating]);

  // Signing in on the consent page with SSO leaves the SPA and comes back
  // to `/`: remember where to return.
  useEffect(() => {
    if (auth === "anon" && join === "approve") {
      rememberApproveReturn(`${window.location.pathname}${window.location.search}`);
    }
  }, [auth, join]);

  /** Leaves a Join page or the welcome screen for a Settings section. */
  function openSettings(tab: SettingsTab) {
    setJoin(null);
    setWelcome(null);
    navigate("settings", tab);
  }

  function onLoginSuccess(next: AuthSession) {
    if (joinPage() != null) {
      if (joinPage() === "approve") takeApproveReturn();
      setSession(next);
      setAuth("authed");
      return;
    }
    const fromInvite = isInvitePath();
    const resolved = resolveView(
      fromInvite ? "/" : window.location.pathname,
      next.default_view,
    );
    setSession(next);
    setView(resolved);
    if (fromInvite) {
      window.history.replaceState(null, "", "/");
    } else {
      syncUrlToView(resolved, "replace");
    }
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

  async function onEndElevation() {
    try {
      await endElevate();
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
    return <AppLoading />;
  }

  if (auth === "anon" || !session) {
    return <LoginPage onSuccess={onLoginSuccess} />;
  }

  // Before any join page: a consent needs an enrolled second factor, and
  // the address (with its query) is kept, so the join resumes after enrolling.
  if (needsSecondFactorEnrollment(session)) {
    return (
      <PageSuspense>
        <MfaEnrollDialog
          role={session.role}
          onEnrolled={refreshSession}
          onLoggedOut={onLogout}
        />
      </PageSuspense>
    );
  }

  if (join === "approve") {
    return (
      <PageSuspense>
        <JoinApprovePage
          onSignOut={async () => {
            await logout().catch(() => undefined);
            onLogout();
          }}
        />
      </PageSuspense>
    );
  }

  if (join === "callback") {
    return (
      <PageSuspense>
        <JoinCallbackPage onOpenLibrary={() => openSettings("clusters")} />
      </PageSuspense>
    );
  }

  if (welcome) {
    return (
      <PageSuspense>
        <WelcomeScreen
          defaultBranchName={welcome.defaultBranchName}
          onStartNew={() => {
            dismissWelcome();
            openSettings("users");
          }}
          onJoined={() => openSettings("clusters")}
        />
      </PageSuspense>
    );
  }

  const canAcquire = session.can_acquire;
  const role = session.role;
  const nav = { view, onNavigate: navigate, defaultView: session.default_view, role };

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
        key={settingsTab ?? "settings"}
        initialTab={settingsTab}
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
  } else if (view === "status" && showStatusNav(role)) {
    page = <StatusPage onLogout={onLogout} nav={nav} role={role} />;
  }

  const impersonatedRole = formatAuthRole(
    session.user?.role ?? session.role,
  );

  return (
    <PreferencesProvider
      defaultView={session.default_view}
      onDefaultViewChange={(v) =>
        setSession((s) => (s ? { ...s, default_view: v } : s))
      }
    >
      <div className="flex h-full min-h-0 flex-col">
        {session.elevated ? (
          <div className="sticky top-0 z-40 flex shrink-0 items-center justify-between gap-3 bg-teal px-4 py-2 text-sm text-ink">
            <span>
              Owner elevation active
              {session.user?.display_name || session.user?.id ? (
                <>
                  {" "}
                  for{" "}
                  <strong>
                    {session.user.display_name?.trim() || `user #${session.user.id}`}
                  </strong>
                </>
              ) : null}
            </span>
            <Button
              type="button"
              variant="secondary"
              className="h-8 bg-white text-[#0b3553] hover:bg-white/90"
              onClick={() => void onEndElevation()}
            >
              End elevation
            </Button>
          </div>
        ) : null}
        {session.impersonating ? (
          <div className="sticky top-0 z-40 flex shrink-0 items-center justify-between gap-3 bg-brick px-4 py-2 text-sm text-white">
            <span>
              Impersonating{" "}
              <strong>
                {session.impersonating.display_name?.trim() ||
                  `user #${session.impersonating.user_id}`}
              </strong>
              {impersonatedRole ? (
                <>
                  {" "}
                  as {impersonatedRole}
                </>
              ) : null}
            </span>
            <Button
              type="button"
              variant="secondary"
              className="h-8 bg-white text-[#c84a34] hover:bg-white/90"
              onClick={() => void onStopImpersonate()}
            >
              Stop
            </Button>
          </div>
        ) : null}
        <div className="min-h-0 flex-1 overflow-hidden">
          <PageSuspense>{page}</PageSuspense>
        </div>
      </div>
    </PreferencesProvider>
  );
}
