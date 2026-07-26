import { useEffect, useState } from "react";
import { LibraryPage } from "@/components/LibraryPage";
import { LoginPage } from "@/components/LoginPage";
import { authMe } from "@/lib/api";

type AuthState = "loading" | "anon" | "authed";

export default function App() {
  const [auth, setAuth] = useState<AuthState>("loading");

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const ok = await authMe();
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
