import { useCallback, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Check, Copy, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  formatCountdown,
  joinErrorMessage,
  listBranchInvites,
  mintBranchInvite,
  revokeBranchInvite,
  type BranchInviteView,
} from "@/lib/join";
import { cn } from "@/lib/utils";

/** Props for {@link AddBranchDialog}. */
export interface AddBranchDialogProps {
  open: boolean;
  onClose: () => void;
  /**
   * The user who will own the branch; omit for the signed-in user (the
   * operator must name one).
   */
  userId?: number;
  /** Their name, for the title. */
  userLabel?: string;
}

/** Ticks once a second while `active`. */
function useNow(active: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [active]);
  return now;
}

/**
 * Add a branch: mints a branch invite and shows it **once**, with a Copy
 * button and a countdown to expiry. The invite lives only in this
 * component's state (never storage, the URL, or a log) and is dropped when
 * the dialog closes. Below it, the user's outstanding invites with Revoke
 * (no secrets).
 */
export function AddBranchDialog({ open, onClose, userId, userLabel }: AddBranchDialogProps) {
  const [label, setLabel] = useState("");
  const [minted, setMinted] = useState<{ url: string; expiresAt: string } | null>(null);
  const [copied, setCopied] = useState(false);
  const [invites, setInvites] = useState<BranchInviteView[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const now = useNow(open);
  // Bumped each time the dialog opens, closes or changes user: an answer
  // that arrives after that belongs to an earlier lifetime and is dropped,
  // so one user's invite is never shown under another's name.
  const lifetime = useRef(0);

  const reload = useCallback(async () => {
    const mine = lifetime.current;
    try {
      // Without a user, the signed-in user's own invites: an Owner may see
      // everyone's, but this dialog is about their own branches.
      const body = await listBranchInvites(userId == null);
      if (lifetime.current !== mine) return;
      setInvites(
        body.links.filter(
          (link) => (userId == null || link.for_user_id === userId) && link.state === "pending",
        ),
      );
    } catch (err) {
      if (lifetime.current !== mine) return;
      setError(joinErrorMessage(err, "Could not load invites."));
    }
  }, [userId]);

  useEffect(() => {
    lifetime.current += 1;
    // A request of the earlier lifetime no longer clears this, and the
    // last user's invites are not shown (or revocable) under the next one.
    setBusy(false);
    setInvites([]);
    if (!open) {
      // Drop the shown invite as soon as the dialog closes.
      setMinted(null);
      setCopied(false);
      setLabel("");
      setError(null);
      return;
    }
    void reload();
  }, [open, reload]);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  async function onMint() {
    const mine = lifetime.current;
    setBusy(true);
    setError(null);
    setCopied(false);
    try {
      const body = await mintBranchInvite({
        user_id: userId,
        label: label.trim() || undefined,
      });
      // Closed (or reopened for someone else) while minting: the invite
      // stays outstanding in that user's list, but is not shown here.
      if (lifetime.current !== mine) return;
      setMinted({ url: body.url, expiresAt: body.link.expires_at });
      await reload();
    } catch (err) {
      if (lifetime.current !== mine) return;
      setError(joinErrorMessage(err, "Could not make an invite."));
    } finally {
      if (lifetime.current === mine) setBusy(false);
    }
  }

  async function onCopy() {
    if (!minted) return;
    try {
      await navigator.clipboard.writeText(minted.url);
      setCopied(true);
    } catch {
      setError("Copy failed: select the invite and copy it by hand.");
    }
  }

  async function onRevoke(linkId: string) {
    const mine = lifetime.current;
    setBusy(true);
    setError(null);
    try {
      await revokeBranchInvite(linkId);
      if (lifetime.current !== mine) return;
      await reload();
    } catch (err) {
      if (lifetime.current !== mine) return;
      setError(joinErrorMessage(err, "Could not revoke the invite."));
    } finally {
      if (lifetime.current === mine) setBusy(false);
    }
  }

  if (!open) return null;
  const expired = minted ? Date.parse(minted.expiresAt) <= now : false;

  return createPortal(
    <div className="fixed inset-0 z-[70] flex items-center justify-center p-4" role="presentation">
      <button
        type="button"
        className="absolute inset-0 bg-scrim backdrop-blur-[2px]"
        aria-label="Close"
        onClick={onClose}
      />
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="add-branch-title"
        className="relative w-full max-w-lg space-y-5 rounded-lg border border-ink/10 bg-paper p-6 shadow-xl"
      >
        <div className="flex items-start justify-between gap-3">
          <div className="space-y-1">
            <h2 id="add-branch-title" className="text-lg font-semibold text-ink">
              Add a branch{userLabel ? ` for ${userLabel}` : ""}
            </h2>
            <p className="text-sm text-ink/60">
              A branch invite lets another Bookclerk server join this library as a branch owned by{" "}
              {userLabel ?? "you"}. Paste it in that server's <strong>Join a library</strong> →{" "}
              <strong>I have an invite</strong>. It works once.
            </p>
          </div>
          <Button type="button" variant="ghost" onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" />
          </Button>
        </div>

        {minted ? (
          <div className="space-y-2 rounded-md border border-teal/30 bg-teal/5 p-3">
            <p className="text-sm font-medium text-ink">
              Copy it now: it is shown only once.
            </p>
            <div className="flex gap-2">
              <Input
                readOnly
                value={minted.url}
                aria-label="Branch invite"
                className="font-mono text-xs"
                onFocus={(e) => e.currentTarget.select()}
              />
              <Button type="button" variant="secondary" onClick={() => void onCopy()}>
                {copied ? <Check className="h-4 w-4" /> : <Copy className="h-4 w-4" />}
                {copied ? "Copied" : "Copy"}
              </Button>
            </div>
            <p className={cn("text-xs", expired ? "text-brick" : "text-ink/55")}>
              {expired
                ? "This invite has expired."
                : `Expires in ${formatCountdown(minted.expiresAt, now)}`}
            </p>
          </div>
        ) : (
          <div className="space-y-2">
            <label className="block text-sm font-medium text-ink" htmlFor="add-branch-label">
              Label (optional)
            </label>
            <div className="flex gap-2">
              <Input
                id="add-branch-label"
                placeholder="Den PC"
                maxLength={64}
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                disabled={busy}
              />
              <Button type="button" disabled={busy} onClick={() => void onMint()}>
                Make invite
              </Button>
            </div>
          </div>
        )}

        {error ? (
          <p className="text-sm font-medium text-brick" role="alert">
            {error}
          </p>
        ) : null}

        <div className="space-y-2">
          <h3 className="text-sm font-semibold text-ink">Outstanding invites</h3>
          {invites.length === 0 ? (
            <p className="text-sm text-ink/50">None.</p>
          ) : (
            <ul className="divide-y divide-ink/10 rounded-md border border-ink/10 bg-card">
              {invites.map((invite) => (
                <li key={invite.link_id} className="flex items-center gap-2 px-3 py-2 text-sm">
                  <span className="min-w-0 flex-1 truncate text-ink">
                    {invite.label || "Branch invite"}
                  </span>
                  <Badge className="bg-ink/5 normal-case tracking-normal text-ink/60">
                    expires in {formatCountdown(invite.expires_at, now)}
                  </Badge>
                  <Button
                    type="button"
                    variant="danger"
                    className="h-8"
                    disabled={busy}
                    onClick={() => void onRevoke(invite.link_id)}
                  >
                    Revoke
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}
