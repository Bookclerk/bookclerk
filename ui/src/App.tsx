import { useEffect, useState } from "react";
import { LibraryPage } from "@/components/LibraryPage";
import { LoginPage } from "@/components/LoginPage";
import { authMe, login } from "@/lib/api";

type AuthState = "loading" | "anon" | "authed";

async function tryTauriAutoLogin(): Promise<boolean> {
  try {
    const { invoke } = await import("@tauri-apps/api/core");
    const token = await invoke<string | null>("operator_token");
    if (!token) return false;
    await login(token);
    return true;
  } catch {
    return false;
  }
}

export default function App() {
  const [auth, setAuth] = useState<AuthState>("loading");

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        let ok = await authMe();
        if (!ok) {
          ok = await tryTauriAutoLogin();
        }
        if (!cancelled) setAuth(ok ? "authed" : "anon");
      } catch {
        if (!cancelled) setAuth("anon");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (auth === "loading") {
    return (
      <div className="flex min-h-full items-center justify-center text-sm text-ink/60">
        Loading Bookclerk…
      </div>
    );
  }

  if (auth === "anon") {
    return <LoginPage onSuccess={() => setAuth("authed")} />;
  }

  return <LibraryPage onLogout={() => setAuth("anon")} />;
}
