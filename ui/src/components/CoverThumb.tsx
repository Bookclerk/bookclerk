import { useState } from "react";
import { cn } from "@/lib/utils";

const FALLBACK_COVER = "/bookclerk-mark.svg";

/** Square audiobook cover thumbnail (Discover / Wishlist / list rows). */
export function CoverThumb({
  url,
  className = "h-12 w-12",
}: {
  url?: string | null;
  className?: string;
}) {
  const src = url?.trim() || null;
  const [failed, setFailed] = useState(false);
  const useFallback = !src || failed;
  const frame = cn(
    "shrink-0 overflow-hidden rounded-md bg-ink/5 shadow-sm",
    className,
  );
  return (
    <img
      src={useFallback ? FALLBACK_COVER : src}
      alt=""
      loading="lazy"
      referrerPolicy="no-referrer"
      aria-hidden
      className={cn(
        frame,
        useFallback ? "object-contain p-2.5 opacity-70" : "object-cover",
      )}
      onError={() => {
        if (!useFallback) setFailed(true);
      }}
    />
  );
}
