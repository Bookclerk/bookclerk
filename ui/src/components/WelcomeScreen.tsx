import { useState } from "react";
import { JoinShell } from "@/components/JoinShell";
import { JoinWizard } from "@/components/JoinWizard";
import { Button } from "@/components/ui/button";

/** Props for {@link WelcomeScreen}. */
export interface WelcomeScreenProps {
  /** Suggested branch name for Join a library. */
  defaultBranchName?: string;
  /** "Start a new library": carry on to the first-run setup. */
  onStartNew: () => void;
  /** An invite joined this install to a library. */
  onJoined: () => void;
}

/**
 * First start of a fresh install (operator only): start a new library, or
 * join an existing one as a branch.
 */
export function WelcomeScreen({ defaultBranchName, onStartNew, onJoined }: WelcomeScreenProps) {
  const [joining, setJoining] = useState(false);
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
            Set up the first administrator, then add your store accounts.
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
