import { UserAvatar, userDisplayLabel } from "@/components/UserAvatar";
import type { QueueWisher } from "@/lib/api";
import { cn } from "@/lib/utils";

const DEFAULT_VISIBLE = 5;

/**
 * Best available public label for a global-queue / Discover wisher.
 *
 * @param wisher - Compact person row from the daemon.
 * @returns Display name, login, or "Operator".
 */
export function wisherLabel(wisher: QueueWisher): string {
  if (wisher.operator) return "Operator";
  return userDisplayLabel({
    display_name: wisher.display_name,
    login_name: wisher.login_name,
  });
}

/**
 * Overlapping tiny avatars for people who wishlisted a title.
 *
 * @param props - Wishers, optional total count, and size class.
 */
export function WisherAvatars({
  wishers,
  wishCount,
  max = DEFAULT_VISIBLE,
  className,
  avatarClassName,
}: {
  wishers: QueueWisher[];
  wishCount?: number;
  max?: number;
  className?: string;
  avatarClassName?: string;
}) {
  if (wishers.length === 0) return null;
  const visible = wishers.slice(0, max);
  const total = Math.max(wishCount ?? 0, wishers.length);
  const overflow = Math.max(0, total - visible.length);
  const labels = wishers.map(wisherLabel);
  const sizeClass = avatarClassName ?? "h-5 w-5 text-[9px] ring-1 ring-paper";
  return (
    <span
      className={cn("flex items-center", className)}
      aria-label={`Wishlisted by ${labels.join(", ")}`}
    >
      <span className="isolate flex -space-x-1.5 overflow-visible">
        {visible.map((wisher, index) => {
          const label = wisherLabel(wisher);
          const ssoPictures = wisher.picture_url
            ? [
                {
                  identity_id: wisher.identity_id ?? 0,
                  provider: "sso",
                  picture_url: wisher.picture_url,
                },
              ]
            : [];
          return (
            <span
              key={wisher.user_id ?? wisher.identity_id ?? `op-${index}`}
              title={label}
              className="relative z-0 origin-center transition-transform duration-200 ease-out hover:z-20 hover:scale-[1.35]"
            >
              <UserAvatar
                userId={wisher.user_id ?? undefined}
                label={label}
                hasAvatar={wisher.has_avatar}
                avatarSource={wisher.avatar_source}
                gravatarHash={wisher.gravatar_hash}
                ssoPictures={ssoPictures}
                className={cn(
                  "rounded-full shadow-sm ring-1 ring-paper",
                  sizeClass,
                )}
              />
            </span>
          );
        })}
        {overflow > 0 ? (
          <span
            className={cn(
              "relative z-[1] flex origin-center items-center justify-center rounded-full bg-fold font-semibold text-ink shadow-sm ring-1 ring-paper transition-transform duration-200 ease-out hover:z-20 hover:scale-[1.35]",
              sizeClass,
            )}
            title={`${overflow} more`}
          >
            +{overflow}
          </span>
        ) : null}
      </span>
    </span>
  );
}
