import { useState } from "react";
import { ClerkSignInForm } from "@/components/ClerkCardsPanel";
import { JoinShell } from "@/components/JoinShell";
import { JoinWizard } from "@/components/JoinWizard";
import { Button } from "@/components/ui/button";
import { isDesktopWindow } from "@/lib/desktop";

/** Props for {@link WelcomeScreen}. */
export interface WelcomeScreenProps {
  /** Suggested branch name for Join a library, and this computer's name for a library card. */
  defaultBranchName?: string;
  /** "Start a new library": carry on to the first-run setup. */
  onStartNew: () => void;
  /** An invite joined this install to a library. */
  onJoined: () => void;
  /** Whether the app runs in the desktop window (defaults to detecting it). */
  desktop?: boolean;
}

/**
 * First start of a fresh install (operator only): start a new library, or
 * join an existing one as a branch.
 *
 * In the desktop window joining comes first: the window opens on signing
 * in to the library with a library card (the library's address, then
 * signing in as a patron there), and hosting a library on this computer is
 * the alternative below it. A card makes this computer a clerk of that
 * library: it shows the books there and runs none of the library's work.
 * The library sends the browser back to `/clerk/callback`.
 */
export function WelcomeScreen({
  defaultBranchName,
  onStartNew,
  onJoined,
  desktop = isDesktopWindow(),
}: WelcomeScreenProps) {
  const [joining, setJoining] = useState(desktop);
  const [joinedTo, setJoinedTo] = useState<string | null>(null);

  if (joinedTo) {
    return (
      <JoinShell
        title="Connected"
        lead={
          <>
            This server is now a branch of <strong>{joinedTo}</strong>. The library's plugins ask
            before they run here; approve them under Settings → Library.
          </>
        }
      >
        <Button type="button" onClick={onJoined}>
          Open Settings → Library
        </Button>
      </JoinShell>
    );
  }

  if (joining && desktop) {
    return (
      <JoinShell
        title="Sign in to your library"
        lead="Enter your library's address, with its port if it has one, then sign in with your account there. This computer gets a library card: it shows the library's books, yours marked, and runs none of its work."
      >
        <ClerkSignInForm defaultName={defaultBranchName ?? ""} />
        <div className="mt-6 border-t border-ink/10 pt-4">
          <Button type="button" variant="secondary" onClick={onStartNew}>
            Host a library on this computer instead
          </Button>
        </div>
      </JoinShell>
    );
  }

  if (joining) {
    return (
      <JoinShell
        title="Join a library"
        lead="This server becomes a branch: it keeps its own database and its own operator, and runs work the library sends it for your accounts. You approve each of the library's plugins here."
      >
        <JoinWizard
          defaultBranchName={defaultBranchName}
          onJoined={(origin) => setJoinedTo(origin)}
          onCancel={() => setJoining(false)}
        />
      </JoinShell>
    );
  }

  return (
    <JoinShell title="Welcome to Bookclerk" lead="How should this server start?">
      <div className="grid gap-3 sm:grid-cols-2">
        <button
          type="button"
          className="space-y-1 rounded-md border border-ink/15 bg-paper p-4 text-left transition-colors hover:border-teal hover:bg-teal/5"
          onClick={onStartNew}
        >
          <span className="block font-semibold text-ink">Start a new library</span>
          <span className="block text-sm text-ink/60">
            Set up the first Owner, then add your store accounts.
          </span>
        </button>
        <button
          type="button"
          className="space-y-1 rounded-md border border-ink/15 bg-paper p-4 text-left transition-colors hover:border-teal hover:bg-teal/5"
          onClick={() => setJoining(true)}
        >
          <span className="block font-semibold text-ink">Join a library</span>
          <span className="block text-sm text-ink/60">
            Add this server as a branch of a library you already use.
          </span>
        </button>
      </div>
    </JoinShell>
  );
}
