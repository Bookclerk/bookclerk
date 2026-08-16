import { useEffect, useState, type ReactNode } from "react";
import { userAvatarUrl } from "@/lib/api";
import { resolveAvatar, type SsoPicture } from "@/lib/avatar";
import { cn } from "@/lib/utils";

/**
 * Best available public label for a user row (never the sequential id).
 *
 * @param user - Display name, login, and/or email fields.
 * @returns Trimmed name, login, email, or "Unnamed user".
 */
export function userDisplayLabel(user: {
  display_name?: string | null;
  login_name?: string | null;
  email?: string | null;
}): string {
  return (
    user.display_name?.trim() ||
    user.login_name?.trim() ||
    user.email?.trim() ||
    "Unnamed user"
  );
}

/**
 * Circular avatar: upload, last-used SSO, Gravatar, or monogram fallback.
 *
 * Presence / camera overlays should be passed as `children` so they are not
 * clipped by the image `overflow-hidden`.
 */
export function UserAvatar({
  userId,
  label,
  hasAvatar,
  avatarSource,
  gravatarHash,
  ssoPictures,
  cacheKey,
  className,
  children,
}: {
  userId?: number;
  label: string;
  hasAvatar?: boolean;
  avatarSource?: string | null;
  gravatarHash?: string | null;
  ssoPictures?: SsoPicture[];
  cacheKey?: number;
  className?: string;
  children?: ReactNode;
}) {
  const resolved = resolveAvatar({
    has_avatar: hasAvatar,
    avatar_source: avatarSource,
    gravatar_hash: gravatarHash,
    sso_pictures: ssoPictures,
  });
  const [broken, setBroken] = useState(false);
  useEffect(() => {
    setBroken(false);
  }, [
    userId,
    cacheKey,
    hasAvatar,
    avatarSource,
    gravatarHash,
    resolved.kind,
  ]);
  const initial = (label.trim() || "?").charAt(0).toUpperCase();
  let src: string | null = null;
  if (!broken) {
    if (resolved.kind === "upload" && userId != null) {
      src = userAvatarUrl(userId, cacheKey);
    } else if (resolved.kind === "gravatar" || resolved.kind === "sso") {
      src = resolved.src;
    }
  }
  return (
    <div className={cn("relative shrink-0", className)}>
      <div className="flex h-full w-full items-center justify-center overflow-hidden rounded-full bg-fold font-semibold text-ink">
        {src ? (
          <img
            src={src}
            alt=""
            referrerPolicy="no-referrer"
            className="h-full w-full object-cover"
            onError={() => setBroken(true)}
          />
        ) : (
          <span aria-hidden>{initial}</span>
        )}
      </div>
      {children}
    </div>
  );
}
