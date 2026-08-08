/** Client-side unknown-path screen (Vite SPA fallback / in-app navigation). */
export function NotFoundPage() {
  const path = window.location.pathname || "/";

  return (
    <div className="flex min-h-full items-center justify-center px-4 py-10">
      <div className="w-full max-w-md animate-[fadeUp_420ms_ease-out]">
        <img
          src="/bookclerk-logo.svg"
          alt="Bookclerk"
          className="mb-8 h-auto w-full"
        />
        <p className="text-xs font-semibold uppercase tracking-wider text-brick">
          404 · Not Found
        </p>
        <h1 className="mt-2 font-display text-3xl font-bold tracking-tight text-ink">
          Not Found
        </h1>
        <p className="mt-2 text-sm text-ink/70">
          No Bookclerk page at{" "}
          <code className="rounded bg-fold/60 px-1 py-0.5 text-[13px]">
            {path}
          </code>
          .
        </p>
        <a
          href="/"
          className="mt-5 inline-flex w-full items-center justify-center rounded-md bg-ink px-3.5 py-2 text-sm font-semibold text-paper shadow-sm hover:bg-ink-soft"
        >
          Open library
        </a>
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
