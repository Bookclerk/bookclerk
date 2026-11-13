import { useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { JoinWizard } from "@/components/JoinWizard";
import { Button } from "@/components/ui/button";
import { isDesktopWindow } from "@/lib/desktop";
import { makeWindowOf } from "@/lib/join";

/** Props for {@link WelcomeScreen}. */
export interface WelcomeScreenProps {
  /** Suggested branch name for Join a library. */
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
 * In the desktop window joining comes first: the window opens on the join
 * form (the library's address, then signing in as a member), and hosting a
 * library on this computer is the alternative below it. A join there makes
 * this computer a window of the library (downloading off); the sign-in path
 * does that on `/join/callback`, an invite does it here.
 */
export function WelcomeScreen({
  defaultBranchName,
  onStartNew,
  onJoined,
  desktop = isDesktopWindow(),
}: WelcomeScreenProps) {
  const [joining, setJoining] = useState(desktop);
  const [joinedTo, setJoinedTo] = useState<string | null>(null);
  // Desktop window only: whether the invite join became a window.
  const [asWindow, setAsWindow] = useState<boolean | null>(null);

  if (joinedTo && asWindow != null) {
    return (
      <JoinShell
        title="Connected"
        lead={
          asWindow ? (
            <>
              This computer is now a window of <strong>{joinedTo}</strong>: it shows your books
              there and downloads nothing. To keep copies here too, turn on "Use this computer to
              download" in Settings → Library.
            </>
          ) : (
            <>
              This computer joined <strong>{joinedTo}</strong>, but downloading could not be turned
              off, so it is a branch that downloads. Turn off "Use this computer to download" in
              Settings → Library to make it a window.
            </>
          )
        }
      >
        <Button type="button" onClick={onJoined}>
          Open Settings → Library
        </Button>
      </JoinShell>
    );
  }

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
        title="Join your library"
        lead="Enter your library's address, with its port if it has one, then sign in as a member. This computer becomes a window of that library: it shows your books there and downloads nothing until you turn downloading on."
      >
        <JoinWizard
          defaultBranchName={defaultBranchName}
          onJoined={(origin, link) => {
            void makeWindowOf(link).then((ok) => {
              setAsWindow(ok);
              setJoinedTo(origin);
            });
          }}
        />
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
