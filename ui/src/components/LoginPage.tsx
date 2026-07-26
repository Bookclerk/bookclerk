import { useState, type FormEvent } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { login } from "@/lib/api";

export function LoginPage({ onSuccess }: { onSuccess: () => void }) {
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function onSubmit(e: FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(token.trim());
      onSuccess();
    } catch {
      setError("Invalid operator token.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <form
        onSubmit={onSubmit}
        className="w-full max-w-md animate-[fadeUp_420ms_ease-out]"
      >
        <img
          src="/bookclerk-logo.svg"
          alt="Bookclerk"
          className="mb-8 h-12 w-auto"
        />
        <h1 className="font-display text-3xl font-bold tracking-tight text-ink">
          Operator sign-in
        </h1>
        <p className="mt-2 text-sm text-ink/70">
          Paste the operator API token from{" "}
          <code className="rounded bg-fold/60 px-1 py-0.5 text-[13px]">
            operator.token
          </code>{" "}
          (or <code className="rounded bg-fold/60 px-1 py-0.5 text-[13px]">BOOKCLERK_OPERATOR_TOKEN</code>).
        </p>
        <label className="mt-6 block text-sm font-semibold" htmlFor="token">
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
      <style>{`
        @keyframes fadeUp {
          from { opacity: 0; transform: translateY(10px); }
          to { opacity: 1; transform: translateY(0); }
        }
      `}</style>
    </div>
  );
}
