import type { ReactNode } from "react";
import { BookclerkLogo } from "@/components/BookclerkLogo";

/**
 * Full-page frame for the welcome screen and the Join a library pages.
 *
 * @param props - Title, optional lead text, and the page body.
 */
export function JoinShell({
  title,
  lead,
  children,
}: {
  title: string;
  lead?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex min-h-full items-start justify-center overflow-auto bg-paper px-4 py-10 sm:items-center">
      <main className="w-full max-w-xl space-y-6 rounded-lg border border-ink/10 bg-card p-6 shadow-sm sm:p-8">
        <BookclerkLogo className="h-9 w-auto" />
        <div className="space-y-2">
          <h1 className="font-display text-2xl font-semibold tracking-tight text-ink">{title}</h1>
          {lead ? <div className="text-sm text-ink/65">{lead}</div> : null}
        </div>
        {children}
      </main>
    </div>
  );
}
