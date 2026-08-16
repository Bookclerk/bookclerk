import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type Dispatch,
  type FormEvent,
  type ReactNode,
  type SetStateAction,
} from "react";
import { createPortal } from "react-dom";
import {
  Check,
  Copy,
  Headphones,
  MoreHorizontal,
  Search,
  UserPlus,
  X,
} from "lucide-react";
import { AvatarPickerDialog } from "@/components/AvatarPickerDialog";
import { UserAvatar, userDisplayLabel } from "@/components/UserAvatar";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  bootstrapAdministrator,
  createUser,
  deleteProfileAvatar,
  deleteUser,
  isApiError,
  listUsers,
  mintUserClaimTicket,
  patchProfile,
  patchUser,
  resetUserPassword,
  startImpersonate,
  uploadProfileAvatar,
  type AuthSession,
  type ListedUser,
} from "@/lib/api";
import { cn } from "@/lib/utils";
import { isOptionalEmailValid } from "@/lib/email";
import { WaveformThrobber } from "@/components/WaveformThrobber";

const EMPTY_USER_FORM = {
  role: "member",
  display_name: "",
  login_name: "",
  email: "",
  password: "",
  mint_invite: true,
};

const EMPTY_BOOTSTRAP_FORM = {
  display_name: "",
  login_name: "",
  email: "",
  password: "",
};

const selectClassName =
  "h-9 w-full rounded-md border border-ink/15 bg-card-strong px-2 text-sm text-ink outline-none focus:border-teal";

function formatWhen(iso: string | null | undefined): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}

function statusBadgeClass(status: string): string {
  switch (status) {
    case "active":
      return "bg-teal/15 text-ink";
    case "disabled":
      return "bg-brick/10 text-brick";
    default:
      return "bg-ink/10 text-ink/70";
  }
}

/** Session activity younger than this counts as currently active. */
const PRESENCE_ACTIVE_MS = 5 * 60 * 1000;
/** Unfinished listen younger than this counts as current playback. */
const PLAYBACK_NOW_MS = 5 * 60 * 1000;
/** Refresh presence while the user-management panel is open. */
const PRESENCE_POLL_MS = 30_000;

type PresenceKind =
  | "disabled"
  | "listening"
  | "active"
  | "idle"
  | "offline"
  | "invited";

/**
 * Milliseconds since `iso`, or null when the timestamp is missing/invalid.
 *
 * @param iso - RFC 3339 timestamp from the users API.
 * @returns Age in milliseconds, or null.
 */
function ageMs(iso: string | null | undefined): number | null {
  if (!iso) return null;
  const t = new Date(iso).getTime();
  if (Number.isNaN(t)) return null;
  return Date.now() - t;
}

/**
 * Whether unfinished listening was updated within the current-playback window.
 *
 * @param user - Listed user row.
 * @returns True when the waveform badge should show.
 */
function isPlaybackNow(user: ListedUser): boolean {
  const ago = ageMs(user.listening?.last_listened_at);
  return ago != null && ago <= PLAYBACK_NOW_MS;
}

/**
 * Presence kind for the avatar badge (disabled, then playback, then session).
 *
 * @param user - Listed user row.
 * @returns Disabled, listening, active, idle, offline, or invited.
 */
function presenceKind(user: ListedUser): PresenceKind {
  if (user.status === "disabled") return "disabled";
  if (isPlaybackNow(user)) return "listening";
  const ago = ageMs(user.last_active_at);
  if (user.online && ago != null && ago <= PRESENCE_ACTIVE_MS) return "active";
  if (user.online) return "idle";
  if (user.last_seen_at) return "offline";
  return "invited";
}

/**
 * Tooltip / activity label for a presence kind.
 *
 * @param user - Listed user row.
 * @returns Human-readable presence text.
 */
function presenceLabel(user: ListedUser): string {
  switch (presenceKind(user)) {
    case "disabled":
      return "Disabled";
    case "listening": {
      const title = user.listening?.title?.trim();
      return title ? `Listening to ${title}` : "Listening";
    }
    case "active":
      return "Active";
    case "idle":
      return "Idle";
    case "invited":
      return "Never signed in";
    default:
      return "Offline";
  }
}

/**
 * Avatar corner badge: dashed invite, grey offline, orange idle, teal active,
 * waveform when playing, red X when disabled.
 *
 * @param props - Listed user and optional size for the list vs detail avatar.
 */
function UserPresenceIndicator({
  user,
  size = "sm",
}: {
  user: ListedUser;
  size?: "sm" | "md";
}) {
  const kind = presenceKind(user);
  const label = presenceLabel(user);
  if (kind === "disabled") {
    return (
      <span
        className="absolute -bottom-0.5 -right-0.5 flex items-center justify-center rounded-full bg-paper"
        title={label}
        aria-label={label}
      >
        <X
          className={cn("text-brick", size === "md" ? "h-3.5 w-3.5" : "h-3 w-3")}
          strokeWidth={3}
          aria-hidden
        />
      </span>
    );
  }
  if (kind === "listening") {
    return (
      <span
        className="absolute -bottom-0.5 -right-0.5 flex items-center justify-center rounded-sm bg-paper"
        title={label}
        aria-label={label}
      >
        <WaveformThrobber size="xs" className="text-teal" />
      </span>
    );
  }
  if (kind === "invited") {
    return (
      <span
        className={cn(
          "absolute rounded-full border-2 border-dashed border-ink/40 bg-transparent",
          size === "md"
            ? "bottom-0 right-0 h-3.5 w-3.5"
            : "bottom-0 right-0 h-3 w-3",
        )}
        title={label}
        aria-label={label}
      />
    );
  }
  return (
    <span
      className={cn(
        "absolute rounded-full border-2 border-paper",
        size === "md"
          ? "bottom-0.5 right-0.5 h-3 w-3"
          : "bottom-0 right-0 h-2.5 w-2.5",
        kind === "active" && "bg-teal",
        kind === "idle" && "bg-brick",
        kind === "offline" && "bg-ink/25",
      )}
      title={label}
      aria-label={label}
    />
  );
}

/** Roles the current session may assign to others. */
function assignableRoles(session: AuthSession | null): string[] {
  if (!session) return [];
  if (session.role === "operator" || session.elevated) {
    return ["member", "administrator", "owner"];
  }
  if (session.role === "owner") {
    return ["member", "administrator"];
  }
  if (session.role === "administrator") {
    return ["member"];
  }
  return [];
}

function canManageListedUser(
  session: AuthSession | null,
  target: ListedUser,
  currentUserId?: number,
): boolean {
  if (currentUserId != null && target.id === currentUserId) {
    return true;
  }
  return assignableRoles(session).includes(target.role);
}

const ACTIONS_MENU_MIN_WIDTH_PX = 176;

/**
 * Overflow actions menu portaled to `document.body` so a parent `overflow-hidden`
 * card cannot clip it. Dismisses on pointer-down outside, Escape, scroll, or resize.
 */
function ActionsOverflowMenu({
  open,
  ariaLabel,
  onOpenChange,
  children,
}: {
  open: boolean;
  ariaLabel: string;
  onOpenChange: (open: boolean) => void;
  children: ReactNode;
}) {
  const triggerRef = useRef<HTMLSpanElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState<{ top: number; left: number } | null>(null);

  const updatePosition = useCallback(() => {
    const trigger = triggerRef.current;
    if (!trigger) return;
    const rect = trigger.getBoundingClientRect();
    const menuH = menuRef.current?.offsetHeight ?? 180;
    const menuW = Math.max(
      ACTIONS_MENU_MIN_WIDTH_PX,
      menuRef.current?.offsetWidth ?? ACTIONS_MENU_MIN_WIDTH_PX,
    );
    const margin = 8;
    const gap = 4;
    const left = Math.min(
      Math.max(margin, rect.right - menuW),
      window.innerWidth - menuW - margin,
    );
    const below = rect.bottom + gap;
    const above = rect.top - gap - menuH;
    const fitsBelow = below + menuH <= window.innerHeight - margin;
    const top = !fitsBelow && above >= margin ? above : below;
    setPos({ top, left });
  }, []);

  useLayoutEffect(() => {
    if (!open) {
      setPos(null);
      return;
    }
    updatePosition();
  }, [open, updatePosition, children]);

  useEffect(() => {
    if (!open) return;
    function onPointerDown(event: PointerEvent) {
      const target = event.target as Node;
      if (triggerRef.current?.contains(target) || menuRef.current?.contains(target)) {
        return;
      }
      onOpenChange(false);
    }
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") onOpenChange(false);
    }
    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKey);
    window.addEventListener("resize", updatePosition);
    window.addEventListener("scroll", updatePosition, true);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKey);
      window.removeEventListener("resize", updatePosition);
      window.removeEventListener("scroll", updatePosition, true);
    };
  }, [open, onOpenChange, updatePosition]);

  return (
    <div
      className="relative flex items-start justify-end"
      onClick={(event) => event.stopPropagation()}
      onKeyDown={(event) => event.stopPropagation()}
    >
      <span ref={triggerRef} className="inline-flex">
        <Button
          type="button"
          variant="ghost"
          className="h-8 w-8 p-0"
          aria-label={ariaLabel}
          aria-expanded={open}
          aria-haspopup="menu"
          onClick={() => onOpenChange(!open)}
        >
          <MoreHorizontal className="h-4 w-4" />
        </Button>
      </span>
      {open
        ? createPortal(
            <div
              ref={menuRef}
              role="menu"
              data-user-actions-menu=""
              className="fixed z-50 min-w-[11rem] rounded-md border border-ink/10 bg-paper py-1 shadow-lg"
              style={
                pos
                  ? { top: pos.top, left: pos.left }
                  : { visibility: "hidden", top: 0, left: 0 }
              }
            >
              {children}
            </div>,
            document.body,
          )
        : null}
    </div>
  );
}

const MENU_ITEM_CLASS =
  "block w-full px-3 py-1.5 text-left text-sm hover:bg-ink/5 disabled:opacity-40";

/**
 * Owner / administrator / operator user provisioning with presence + integrations.
 */
export function UserManagementPanel({
  users,
  setUsers,
  busy,
  setBusy,
  error,
  setError,
  showBootstrap,
  showOperatorChrome,
  session,
  adminCount: _adminCount,
  currentUserId,
  onSessionChange,
  onUsersChanged,
}: {
  users: ListedUser[];
  setUsers: Dispatch<SetStateAction<ListedUser[]>>;
  busy: boolean;
  setBusy: (v: boolean) => void;
  error: string | null;
  setError: (v: string | null) => void;
  showBootstrap: boolean;
  showOperatorChrome: boolean;
  session: AuthSession | null;
  adminCount: number;
  currentUserId?: number;
  onSessionChange?: () => void | Promise<void>;
  onUsersChanged: () => void | Promise<void>;
}) {
  const [createForm, setCreateForm] = useState(EMPTY_USER_FORM);
  const [bootstrapForm, setBootstrapForm] = useState(EMPTY_BOOTSTRAP_FORM);
  const [claimTicket, setClaimTicket] = useState<{
    ticket: string;
    label: string;
    inviteUrl?: string | null;
  } | null>(null);
  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<"all" | "active" | "disabled">(
    "all",
  );
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [impersonateBusy, setImpersonateBusy] = useState(false);
  const [menuOpenId, setMenuOpenId] = useState<number | null>(null);
  const [inviteCopied, setInviteCopied] = useState(false);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [avatarKey, setAvatarKey] = useState(0);

  useEffect(() => {
    if (selectedId == null) return;
    function onPointerDown(event: PointerEvent) {
      const target = event.target;
      if (!(target instanceof Element)) return;
      if (
        target.closest("[data-user-row]") ||
        target.closest("[data-user-details]") ||
        target.closest("[data-user-actions-menu]") ||
        target.closest("[role=dialog]") ||
        target.closest("[data-avatar-picker]") ||
        target.closest("select") ||
        target.tagName === "OPTION"
      ) {
        return;
      }
      setSelectedId(null);
      setMenuOpenId(null);
    }
    document.addEventListener("pointerdown", onPointerDown);
    return () => document.removeEventListener("pointerdown", onPointerDown);
  }, [selectedId]);

  useEffect(() => {
    const id = window.setInterval(() => {
      void listUsers()
        .then(setUsers)
        .catch(() => {
          /* Keep the last successful snapshot if a poll fails. */
        });
    }, PRESENCE_POLL_MS);
    return () => window.clearInterval(id);
  }, [setUsers]);

  useEffect(() => {
    setInviteCopied(false);
  }, [claimTicket?.ticket, claimTicket?.inviteUrl]);

  useEffect(() => {
    if (!inviteCopied) return;
    const timer = window.setTimeout(() => setInviteCopied(false), 2200);
    return () => window.clearTimeout(timer);
  }, [inviteCopied]);

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return users.filter((u) => {
      if (statusFilter !== "all" && u.status !== statusFilter) return false;
      if (!q) return true;
      const hay = [
        u.display_name,
        u.login_name,
        u.email,
        u.role,
        String(u.id),
        ...(u.integrations ?? []).flatMap((i) => [i.source, i.label, i.account_id]),
      ]
        .filter(Boolean)
        .join(" ")
        .toLowerCase();
      return hay.includes(q);
    });
  }, [users, query, statusFilter]);

  const selected = users.find((u) => u.id === selectedId) ?? null;

  const ownerCount = useMemo(
    () => users.filter((u) => u.role === "owner" && u.status === "active").length,
    [users],
  );

  const roleChoices = assignableRoles(session);

  function canMutate(u: ListedUser): boolean {
    return canManageListedUser(session, u, currentUserId);
  }

  const selectedEditable = selected ? canMutate(selected) : false;

  function isDeleteDisabled(u: ListedUser): boolean {
    if (busy) return true;
    if (currentUserId != null && u.id === currentUserId) return true;
    if (!canManageListedUser(session, u, currentUserId)) return true;
    if (u.role === "owner" && u.status === "active" && ownerCount <= 1) {
      return true;
    }
    return false;
  }

  async function copyClaimTicket() {
    if (!claimTicket) return;
    const value = claimTicket.inviteUrl?.trim() || claimTicket.ticket;
    try {
      await navigator.clipboard.writeText(value);
      setInviteCopied(true);
    } catch {
      setInviteCopied(false);
    }
  }

  const canPickOwnAvatar =
    Boolean(selected) &&
    currentUserId != null &&
    selected?.id === currentUserId &&
    !session?.impersonating;

  async function refreshAfterAvatarChange() {
    await Promise.all([onSessionChange?.(), onUsersChanged()]);
  }

  async function onSelectOwnAvatarSource(source: string) {
    setBusy(true);
    setError(null);
    try {
      await patchProfile({ avatar_source: source });
      await refreshAfterAvatarChange();
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to update profile picture");
    } finally {
      setBusy(false);
    }
  }

  async function onUploadOwnAvatar(file: File) {
    if (file.size > 1_500_000) {
      setError("Choose an image smaller than 1.5 MB.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await uploadProfileAvatar(file);
      setAvatarKey(Date.now());
      await refreshAfterAvatarChange();
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to update profile picture");
    } finally {
      setBusy(false);
    }
  }

  async function onRemoveOwnAvatar() {
    setBusy(true);
    setError(null);
    try {
      await deleteProfileAvatar();
      setAvatarKey(Date.now());
      await refreshAfterAvatarChange();
    } catch (err) {
      setError(isApiError(err) ? err.message : "Failed to remove profile picture");
    } finally {
      setBusy(false);
    }
  }

  async function onBootstrap(e: FormEvent) {
    e.preventDefault();
    if (!isOptionalEmailValid(bootstrapForm.email)) {
      setError("Enter a valid email address.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await bootstrapAdministrator({
        display_name: bootstrapForm.display_name.trim() || undefined,
        login_name: bootstrapForm.login_name.trim() || undefined,
        email: bootstrapForm.email.trim() || undefined,
        password: bootstrapForm.password || undefined,
      });
      setClaimTicket({
        ticket: res.claim_ticket,
        inviteUrl: res.invite_url,
        label: "Bootstrap owner invite link",
      });
      setBootstrapForm(EMPTY_BOOTSTRAP_FORM);
      await onUsersChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Bootstrap failed");
    } finally {
      setBusy(false);
    }
  }

  async function onCreateUser(e: FormEvent) {
    e.preventDefault();
    if (!isOptionalEmailValid(createForm.email)) {
      setError("Enter a valid email address.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const res = await createUser({
        role: roleChoices.includes(createForm.role)
          ? createForm.role
          : (roleChoices[0] ?? "member"),
        display_name: createForm.display_name.trim() || undefined,
        login_name: createForm.login_name.trim() || undefined,
        email: createForm.email.trim() || undefined,
        password: createForm.password || undefined,
        mint_invite: createForm.mint_invite,
      });
      if (res.claim_ticket) {
        setClaimTicket({
          ticket: res.claim_ticket,
          inviteUrl: res.invite_url,
          label: `Invite for ${res.user.display_name || res.user.login_name || res.user.email || `user #${res.user.id}`}`,
        });
      }
      setCreateForm(EMPTY_USER_FORM);
      await onUsersChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Create user failed");
    } finally {
      setBusy(false);
    }
  }

  async function onPatchUser(
    id: number,
    patch: {
      role?: string;
      status?: string;
      display_name?: string;
      login_name?: string;
      email?: string;
    },
  ) {
    setBusy(true);
    setError(null);
    try {
      const res = await patchUser(id, patch);
      setUsers((current) =>
        current.map((u) => (u.id === id ? { ...u, ...res.user } : u)),
      );
    } catch (err) {
      if (isApiError(err) && err.status === 409) {
        setError("Cannot demote or disable the last active owner.");
      } else {
        setError(err instanceof Error ? err.message : "Update failed");
      }
      await onUsersChanged();
    } finally {
      setBusy(false);
    }
  }

  async function onResetUserPassword(u: ListedUser) {
    setBusy(true);
    setError(null);
    try {
      const res = await resetUserPassword(u.id);
      setClaimTicket({
        ticket: res.claim_ticket,
        inviteUrl: res.invite_url,
        label: `Password reset for ${u.display_name || u.login_name || `user #${u.id}`}`,
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Password reset failed");
    } finally {
      setBusy(false);
    }
  }

  async function onMintClaimTicket(u: ListedUser) {
    setBusy(true);
    setError(null);
    try {
      const res = await mintUserClaimTicket(u.id);
      setClaimTicket({
        ticket: res.claim_ticket,
        inviteUrl: res.invite_url,
        label: `Invite for ${u.display_name || u.login_name || u.email || `user #${u.id}`}`,
      });
    } catch (err) {
      setError(err instanceof Error ? err.message : "Remint failed");
    } finally {
      setBusy(false);
    }
  }

  async function onDeleteUser(u: ListedUser) {
    if (
      !window.confirm(
        `Delete ${u.display_name || u.login_name || `user #${u.id}`}? Wishlist and store links are removed; acquired books stay.`,
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteUser(u.id);
      if (selectedId === u.id) setSelectedId(null);
      await onUsersChanged();
    } catch (err) {
      if (isApiError(err) && err.status === 409) {
        setError("Cannot delete the last active administrator.");
      } else {
        setError(err instanceof Error ? err.message : "Delete failed");
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <h2 className="text-lg font-semibold text-ink">Users</h2>
        <p className="text-sm text-ink/55">
          Provision accounts, review store integrations, and see who is signed in
          or actively listening.
        </p>
      </div>

      {error ? (
        <p className="text-sm font-medium text-brick" role="alert">
          {error}
        </p>
      ) : null}

      {claimTicket ? (
        <div
          className={cn(
            "rounded-md border px-3 py-2 text-sm text-ink transition-colors duration-300",
            inviteCopied
              ? "border-teal/55 bg-teal/20"
              : "border-teal/25 bg-teal/10",
          )}
        >
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div className="min-w-0 flex-1 space-y-1">
              <p className="font-medium">{claimTicket.label}</p>
              <p className="break-all font-mono text-xs text-ink/80">
                {claimTicket.inviteUrl?.trim() || claimTicket.ticket}
              </p>
              <p
                className={cn(
                  "text-xs",
                  inviteCopied ? "font-medium text-teal" : "text-ink/50",
                )}
                role="status"
                aria-live="polite"
              >
                {inviteCopied
                  ? "Copied to clipboard."
                  : "Copy and send this magic link manually. Email notifications can be added later."}
              </p>
            </div>
            <Button
              type="button"
              variant="secondary"
              className="h-8 shrink-0"
              onClick={() => void copyClaimTicket()}
            >
              {inviteCopied ? (
                <Check className="h-4 w-4" aria-hidden />
              ) : (
                <Copy className="h-4 w-4" aria-hidden />
              )}
              {inviteCopied ? "Copied" : "Copy invite link"}
            </Button>
          </div>
        </div>
      ) : null}

      {showBootstrap ? (
        <form
          className="grid gap-3 bg-card px-3 py-3 sm:grid-cols-[1fr_1fr_1fr_1fr_auto]"
          onSubmit={(e) => void onBootstrap(e)}
        >
          <Input
            aria-label="Owner login name"
            value={bootstrapForm.login_name}
            onChange={(e) =>
              setBootstrapForm((c) => ({ ...c, login_name: e.target.value }))
            }
            placeholder="login name"
            autoComplete="off"
          />
          <Input
            aria-label="Owner email"
            type="email"
            value={bootstrapForm.email}
            aria-invalid={!isOptionalEmailValid(bootstrapForm.email)}
            onChange={(e) =>
              setBootstrapForm((c) => ({ ...c, email: e.target.value }))
            }
            placeholder="email"
            autoComplete="off"
          />
          <Input
            aria-label="Owner display name"
            value={bootstrapForm.display_name}
            onChange={(e) =>
              setBootstrapForm((c) => ({ ...c, display_name: e.target.value }))
            }
            placeholder="display name"
            autoComplete="off"
          />
          <Input
            aria-label="Owner password"
            type="password"
            value={bootstrapForm.password}
            onChange={(e) =>
              setBootstrapForm((c) => ({ ...c, password: e.target.value }))
            }
            placeholder="optional password"
            autoComplete="new-password"
          />
          <Button type="submit" disabled={busy}>
            <UserPlus className="h-4 w-4" />
            {busy ? "Bootstrapping…" : "Bootstrap owner"}
          </Button>
        </form>
      ) : (
        <form
          className="grid gap-3 bg-card px-3 py-3 sm:grid-cols-[9rem_1fr_1fr_1fr_1fr_auto_auto]"
          onSubmit={(e) => void onCreateUser(e)}
        >
          <select
            aria-label="New user role"
            value={roleChoices.includes(createForm.role) ? createForm.role : (roleChoices[0] ?? "member")}
            onChange={(e) =>
              setCreateForm((c) => ({ ...c, role: e.target.value }))
            }
            className={selectClassName}
          >
            {roleChoices.includes("member") ? (
              <option value="member">Member</option>
            ) : null}
            {roleChoices.includes("administrator") ? (
              <option value="administrator">Administrator</option>
            ) : null}
            {roleChoices.includes("owner") ? (
              <option value="owner">Owner</option>
            ) : null}
          </select>
          <Input
            aria-label="New user login name"
            value={createForm.login_name}
            onChange={(e) =>
              setCreateForm((c) => ({ ...c, login_name: e.target.value }))
            }
            placeholder="login name"
            autoComplete="off"
          />
          <Input
            aria-label="New user email"
            type="email"
            value={createForm.email}
            aria-invalid={!isOptionalEmailValid(createForm.email)}
            onChange={(e) =>
              setCreateForm((c) => ({ ...c, email: e.target.value }))
            }
            placeholder="email"
            autoComplete="off"
          />
          <Input
            aria-label="New user display name"
            value={createForm.display_name}
            onChange={(e) =>
              setCreateForm((c) => ({ ...c, display_name: e.target.value }))
            }
            placeholder="display name"
            autoComplete="off"
          />
          <Input
            aria-label="New user password"
            type="password"
            value={createForm.password}
            onChange={(e) =>
              setCreateForm((c) => ({ ...c, password: e.target.value }))
            }
            placeholder="optional password"
            autoComplete="new-password"
          />
          <label className="flex items-center gap-2 text-xs text-ink/70">
            <input
              type="checkbox"
              className="h-4 w-4 accent-teal"
              checked={createForm.mint_invite}
              onChange={(e) =>
                setCreateForm((c) => ({ ...c, mint_invite: e.target.checked }))
              }
            />
            Invite link
          </label>
          <Button type="submit" disabled={busy}>
            <UserPlus className="h-4 w-4" />
            {busy ? "Creating…" : "Add user"}
          </Button>
        </form>
      )}

      <div className="flex flex-wrap items-center gap-2">
        <div className="relative min-w-[14rem] flex-1">
          <Search className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-ink/40" />
          <Input
            className="pl-8"
            aria-label="Search users"
            placeholder="Search name, login, or integration…"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
        </div>
        <select
          aria-label="Filter by status"
          className={cn(selectClassName, "w-auto min-w-[8rem]")}
          value={statusFilter}
          onChange={(e) =>
            setStatusFilter(e.target.value as "all" | "active" | "disabled")
          }
        >
          <option value="all">All statuses</option>
          <option value="active">Active</option>
          <option value="disabled">Disabled</option>
        </select>
        <p className="text-xs text-ink/45">
          {filtered.length} of {users.length} users
        </p>
      </div>

      <div className="grid items-start gap-4 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <div className="max-h-[min(28rem,60vh)] min-w-0 self-start overflow-y-auto overflow-x-hidden rounded-md border border-ink/10 bg-card">
          <div className="sticky top-0 z-10 hidden grid-cols-[minmax(12rem,1.4fr)_6rem_7rem_minmax(8rem,1fr)_7rem_2.5rem] gap-2 border-b border-ink/10 bg-card-strong px-3 py-2 text-[11px] font-semibold uppercase tracking-wide text-ink/45 backdrop-blur-sm sm:grid">
            <span>User</span>
            <span>Status</span>
            <span>Role</span>
            <span>Integrations</span>
            <span>Activity</span>
            <span className="sr-only">Actions</span>
          </div>
          <ul className="divide-y divide-ink/10">
            {filtered.length === 0 ? (
              <li className="px-3 py-8 text-center text-sm text-ink/50">
                No users match this filter.
              </li>
            ) : (
              filtered.map((u) => {
                const selectedRow = selectedId === u.id;
                return (
                  <li key={u.id}>
                    <div
                      role="button"
                      tabIndex={0}
                      data-user-row=""
                      onClick={() => setSelectedId(u.id)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" || e.key === " ") {
                          e.preventDefault();
                          setSelectedId(u.id);
                        }
                      }}
                      className={cn(
                        "grid w-full cursor-pointer gap-2 px-3 py-3 text-left text-sm transition-colors sm:grid-cols-[minmax(12rem,1.4fr)_6rem_7rem_minmax(8rem,1fr)_7rem_2.5rem]",
                        selectedRow
                          ? "border-l-4 border-l-teal bg-teal/10"
                          : "border-l-4 border-l-transparent hover:bg-ink/[0.03]",
                      )}
                    >
                      <div className="flex min-w-0 items-center gap-3">
                        <UserAvatar
                          userId={u.id}
                          label={userDisplayLabel(u)}
                          hasAvatar={u.has_avatar}
                          avatarSource={u.avatar_source}
                          gravatarHash={u.gravatar_hash}
                          ssoPictures={u.sso_pictures}
                          className="h-9 w-9 text-sm"
                        >
                          <UserPresenceIndicator user={u} />
                        </UserAvatar>
                        <div className="min-w-0">
                          <p className="truncate font-medium text-ink">
                            {userDisplayLabel(u)}
                          </p>
                          <p className="truncate text-xs text-ink/50">
                            {u.email?.trim() || u.login_name || "no login"}
                          </p>
                        </div>
                      </div>
                      <div className="flex items-center">
                        <Badge
                          className={cn(
                            "normal-case tracking-normal",
                            statusBadgeClass(u.status),
                          )}
                        >
                          {u.status}
                        </Badge>
                      </div>
                      <div className="flex items-center text-ink/70">
                        {u.role === "owner"
                          ? "Owner"
                          : u.role === "administrator"
                            ? "Admin"
                            : "Member"}
                      </div>
                      <div className="flex flex-wrap items-center gap-1">
                        {(u.integrations ?? []).length === 0 ? (
                          <span className="text-xs text-ink/40">None</span>
                        ) : (
                          (u.integrations ?? []).slice(0, 3).map((i) => (
                            <Badge
                              key={`${i.source}:${i.account_id}`}
                              className="bg-ink/8 text-ink/75 normal-case tracking-normal"
                            >
                              {i.source}
                            </Badge>
                          ))
                        )}
                        {(u.integrations ?? []).length > 3 ? (
                          <span className="text-xs text-ink/45">
                            +{(u.integrations ?? []).length - 3}
                          </span>
                        ) : null}
                      </div>
                      <div className="min-w-0 space-y-1">
                        {u.listening ? (
                          <div className="flex items-start gap-1.5 text-xs text-teal">
                            {isPlaybackNow(u) ? (
                              <WaveformThrobber
                                size="xs"
                                className="mt-0.5 shrink-0 text-teal"
                              />
                            ) : (
                              <Headphones className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                            )}
                            <span className="line-clamp-2">
                              {u.listening.title?.trim() || "Listening"}
                              <span className="text-ink/45">
                                {" "}
                                · {u.listening.provider}
                              </span>
                            </span>
                          </div>
                        ) : (
                          <p className="text-xs text-ink/45">
                            {presenceLabel(u)}
                          </p>
                        )}
                        {u.last_seen_at || u.last_active_at ? (
                          <p className="text-[11px] text-ink/40">
                            {formatWhen(u.last_seen_at ?? u.last_active_at)}
                          </p>
                        ) : null}
                      </div>
                      <ActionsOverflowMenu
                        open={menuOpenId === u.id}
                        ariaLabel={`Actions for user ${u.id}`}
                        onOpenChange={(next) =>
                          setMenuOpenId(next ? u.id : null)
                        }
                      >
                            <button
                              type="button"
                              role="menuitem"
                              className={MENU_ITEM_CLASS}
                              onClick={() => {
                                setMenuOpenId(null);
                                setSelectedId(u.id);
                              }}
                            >
                              View details
                            </button>
                            <button
                              type="button"
                              role="menuitem"
                              className={MENU_ITEM_CLASS}
                              disabled={busy || !canMutate(u)}
                              onClick={() => {
                                setMenuOpenId(null);
                                void onResetUserPassword(u);
                              }}
                            >
                              Reset password
                            </button>
                            <button
                              type="button"
                              role="menuitem"
                              className={MENU_ITEM_CLASS}
                              disabled={busy || u.status === "disabled" || !canMutate(u)}
                              onClick={() => {
                                setMenuOpenId(null);
                                void onMintClaimTicket(u);
                              }}
                            >
                              Remint invite
                            </button>
                            {showOperatorChrome ? (
                              <button
                                type="button"
                                role="menuitem"
                                className={MENU_ITEM_CLASS}
                                disabled={
                                  impersonateBusy ||
                                  session?.impersonating?.user_id === u.id
                                }
                                onClick={() => {
                                  setMenuOpenId(null);
                                  void (async () => {
                                    setImpersonateBusy(true);
                                    try {
                                      await startImpersonate(u.id);
                                      await onSessionChange?.();
                                    } catch (err) {
                                      setError(
                                        err instanceof Error
                                          ? err.message
                                          : "Impersonate failed",
                                      );
                                    } finally {
                                      setImpersonateBusy(false);
                                    }
                                  })();
                                }}
                              >
                                Impersonate
                              </button>
                            ) : null}
                            <button
                              type="button"
                              role="menuitem"
                              className={cn(MENU_ITEM_CLASS, "text-brick hover:bg-brick/5")}
                              disabled={isDeleteDisabled(u)}
                              onClick={() => {
                                setMenuOpenId(null);
                                void onDeleteUser(u);
                              }}
                            >
                              Delete
                            </button>
                      </ActionsOverflowMenu>
                    </div>
                  </li>
                );
              })
            )}
          </ul>
        </div>

        <aside
          data-user-details=""
          className="rounded-md border border-ink/10 bg-card p-4"
        >
          {selected ? (
            <div className="space-y-4">
              <div className="flex items-start justify-between gap-2">
                <div>
                  <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                    User information
                  </p>
                  <div className="mt-2 flex items-center gap-3">
                    {canPickOwnAvatar ? (
                      <button
                        type="button"
                        className="rounded-full focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal"
                        onClick={(e) => {
                          e.stopPropagation();
                          setPickerOpen(true);
                        }}
                        aria-label="Change profile picture"
                      >
                        <UserAvatar
                          userId={selected.id}
                          label={userDisplayLabel(selected)}
                          hasAvatar={selected.has_avatar}
                          avatarSource={selected.avatar_source}
                          gravatarHash={selected.gravatar_hash}
                          ssoPictures={selected.sso_pictures}
                          cacheKey={avatarKey}
                          className="h-12 w-12 text-base"
                        >
                          <UserPresenceIndicator user={selected} size="md" />
                        </UserAvatar>
                      </button>
                    ) : (
                      <UserAvatar
                        userId={selected.id}
                        label={userDisplayLabel(selected)}
                        hasAvatar={selected.has_avatar}
                        avatarSource={selected.avatar_source}
                        gravatarHash={selected.gravatar_hash}
                        ssoPictures={selected.sso_pictures}
                        className="h-12 w-12 text-base"
                      >
                        <UserPresenceIndicator user={selected} size="md" />
                      </UserAvatar>
                    )}
                    <div>
                      <p className="font-semibold text-ink">
                        {userDisplayLabel(selected)}
                      </p>
                      <p className="text-sm text-ink/55">
                        {selected.email?.trim() ||
                          selected.login_name ||
                          "No email"}
                      </p>
                    </div>
                  </div>
                </div>
                <Badge
                  className={cn(
                    "normal-case tracking-normal",
                    statusBadgeClass(selected.status),
                  )}
                >
                  {selected.status}
                </Badge>
              </div>

              {selected.listening ? (
                <div className="rounded-md bg-teal/10 px-3 py-2 text-sm text-ink">
                  <div className="flex items-center gap-2 font-medium">
                    {isPlaybackNow(selected) ? (
                      <WaveformThrobber size="xs" className="text-teal" />
                    ) : (
                      <Headphones className="h-4 w-4 text-teal" />
                    )}
                    {isPlaybackNow(selected)
                      ? "Listening now"
                      : "Recently listening"}
                  </div>
                  <p className="mt-1 text-ink/70">
                    {selected.listening.title?.trim() || "Untitled"}
                    <span className="text-ink/45">
                      {" "}
                      · {selected.listening.provider}
                    </span>
                  </p>
                </div>
              ) : null}

              <div className="space-y-2">
                <label className="block text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Role
                </label>
                <select
                  aria-label={`Role for user ${selected.id}`}
                  value={selected.role}
                  disabled={busy || !selectedEditable}
                  onChange={(e) =>
                    void onPatchUser(selected.id, { role: e.target.value })
                  }
                  className={selectClassName}
                >
                  {!roleChoices.includes(selected.role) ? (
                    <option value={selected.role}>{selected.role}</option>
                  ) : null}
                  {roleChoices.includes("member") ? (
                    <option value="member">Member</option>
                  ) : null}
                  {roleChoices.includes("administrator") ? (
                    <option value="administrator">Administrator</option>
                  ) : null}
                  {roleChoices.includes("owner") ? (
                    <option value="owner">Owner</option>
                  ) : null}
                </select>
              </div>
              <div className="space-y-2">
                <label className="block text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Status
                </label>
                <select
                  aria-label={`Status for user ${selected.id}`}
                  value={selected.status}
                  disabled={busy || !selectedEditable}
                  onChange={(e) =>
                    void onPatchUser(selected.id, { status: e.target.value })
                  }
                  className={selectClassName}
                >
                  <option value="active">Active</option>
                  <option value="disabled">Disabled</option>
                </select>
              </div>
              <div className="space-y-2">
                <label className="block text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Display name
                </label>
                <Input
                  aria-label={`Display name for user ${selected.id}`}
                  value={selected.display_name ?? ""}
                  disabled={busy || !selectedEditable}
                  onChange={(e) => {
                    const value = e.target.value;
                    setUsers((current) =>
                      current.map((user) =>
                        user.id === selected.id
                          ? { ...user, display_name: value }
                          : user,
                      ),
                    );
                  }}
                  onBlur={(e) =>
                    void onPatchUser(selected.id, {
                      display_name: e.target.value,
                    })
                  }
                />
              </div>
              <div className="space-y-2">
                <label className="block text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Login name
                </label>
                <Input
                  aria-label={`Login name for user ${selected.id}`}
                  value={selected.login_name ?? ""}
                  disabled={busy || !selectedEditable}
                  onChange={(e) => {
                    const value = e.target.value;
                    setUsers((current) =>
                      current.map((user) =>
                        user.id === selected.id
                          ? { ...user, login_name: value }
                          : user,
                      ),
                    );
                  }}
                  onBlur={(e) =>
                    void onPatchUser(selected.id, { login_name: e.target.value })
                  }
                />
              </div>

              <div className="space-y-2">
                <label className="block text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Email
                </label>
                <Input
                  aria-label={`Email for user ${selected.id}`}
                  type="email"
                  value={selected.email ?? ""}
                  aria-invalid={!isOptionalEmailValid(selected.email ?? "")}
                  disabled={busy || !selectedEditable}
                  onChange={(e) => {
                    const value = e.target.value;
                    setUsers((current) =>
                      current.map((user) =>
                        user.id === selected.id ? { ...user, email: value } : user,
                      ),
                    );
                  }}
                  onBlur={(e) => {
                    if (!isOptionalEmailValid(e.target.value)) return;
                    void onPatchUser(selected.id, { email: e.target.value });
                  }}
                />
              </div>

              <div className="space-y-2">
                <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Linked identities ({(selected.identities ?? []).length})
                </p>
                {(selected.identities ?? []).length === 0 ? (
                  <p className="text-sm text-ink/50">No linked login identities.</p>
                ) : (
                  <ul className="flex flex-wrap gap-1.5">
                    {(selected.identities ?? []).map((i) => (
                      <li key={`${i.provider}:${i.external_user_id}`}>
                        <Badge className="bg-ink/8 text-ink normal-case tracking-normal">
                          {i.provider}
                          {i.label ? ` · ${i.label}` : ""}
                        </Badge>
                      </li>
                    ))}
                  </ul>
                )}
              </div>

              <div className="space-y-2">
                <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Integrations ({(selected.integrations ?? []).length})
                </p>
                {(selected.integrations ?? []).length === 0 ? (
                  <p className="text-sm text-ink/50">No store connections.</p>
                ) : (
                  <ul className="flex flex-wrap gap-1.5">
                    {(selected.integrations ?? []).map((i) => (
                      <li key={`${i.source}:${i.account_id}`}>
                        <Badge className="bg-ink/8 text-ink normal-case tracking-normal">
                          {i.source}
                          {i.label ? ` · ${i.label}` : ""}
                        </Badge>
                      </li>
                    ))}
                  </ul>
                )}
              </div>

              <p className="text-xs text-ink/45">
                Password: {selected.has_password ? "set" : "not set"}
                {" · "}
                Last seen{" "}
                {selected.last_seen_at || selected.last_active_at
                  ? formatWhen(selected.last_seen_at ?? selected.last_active_at)
                  : "never"}
              </p>
            </div>
          ) : (
            <p className="text-sm text-ink/50">
              Select a user to edit role, status, and review integrations.
            </p>
          )}
        </aside>
      </div>
      {canPickOwnAvatar && selected ? (
        <AvatarPickerDialog
          open={pickerOpen}
          onOpenChange={setPickerOpen}
          userId={selected.id}
          label={userDisplayLabel(selected)}
          fields={{
            has_avatar: selected.has_avatar,
            avatar_source: selected.avatar_source,
            gravatar_hash: selected.gravatar_hash,
            sso_pictures: selected.sso_pictures,
          }}
          cacheKey={avatarKey}
          busy={busy}
          onSelectSource={onSelectOwnAvatarSource}
          onUpload={(file) => void onUploadOwnAvatar(file)}
          onRemoveUpload={() => void onRemoveOwnAvatar()}
        />
      ) : null}
    </div>
  );
}
