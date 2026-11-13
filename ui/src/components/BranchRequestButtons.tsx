import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  availableThroughLabel,
  branchesToAsk,
  isApiError,
  requestFromBranch,
  type BranchAvailability,
} from "@/lib/api";

/** What one branch's request did. */
type AskState = "idle" | "busy" | "sent" | "already" | "gone" | "failed";

/** Props for {@link BranchRequestButtons}. */
export interface BranchRequestButtonsProps {
  /** The title's branch availability. */
  through: BranchAvailability[] | undefined;
}

/**
 * "Available through Riley's branch", with a button per branch that asks
 * it for the book (`POST /api/branch-requests`). The branch takes the
 * request automatically; the book arrives with its next catalog sync.
 * Renders nothing when no branch lists the title.
 */
export function BranchRequestButtons({ through }: BranchRequestButtonsProps) {
  const label = availableThroughLabel(through);
  const branches = branchesToAsk(through);
  const [asked, setAsked] = useState<Record<string, AskState>>({});
  if (!label) return null;

  async function ask(nodeId: string, workId: string) {
    setAsked((prev) => ({ ...prev, [nodeId]: "busy" }));
    try {
      const queued = await requestFromBranch(workId, nodeId);
      setAsked((prev) => ({ ...prev, [nodeId]: queued.already_queued ? "already" : "sent" }));
    } catch (err) {
      const state: AskState = isApiError(err) && err.status === 404 ? "gone" : "failed";
      setAsked((prev) => ({ ...prev, [nodeId]: state }));
    }
  }

  return (
    <div className="space-y-2 rounded-md bg-teal/10 px-3 py-2 text-sm">
      <p className="font-medium text-teal">{label}</p>
      {branches.map((branch) => {
        const state = asked[branch.node_id] ?? "idle";
        return (
          <div key={branch.node_id} className="flex flex-wrap items-center gap-x-3 gap-y-1">
            {state === "sent" || state === "already" ? (
              <span className="text-xs text-ink/70" role="status">
                {state === "sent" ? "Requested" : "Already requested"}. It arrives here after{" "}
                {branch.label} next sends its books.
              </span>
            ) : (
              <Button
                type="button"
                variant="secondary"
                className="h-8"
                disabled={state === "busy"}
                onClick={() => void ask(branch.node_id, branch.work_id)}
              >
                {state === "busy" ? "Requesting…" : `Request from ${branch.label}`}
              </Button>
            )}
            {state === "gone" ? (
              <span className="text-xs text-brick" role="alert">
                {branch.label} no longer lists this book.
              </span>
            ) : null}
            {state === "failed" ? (
              <span className="text-xs text-brick" role="alert">
                Could not send the request. Try again later.
              </span>
            ) : null}
          </div>
        );
      })}
    </div>
  );
}
