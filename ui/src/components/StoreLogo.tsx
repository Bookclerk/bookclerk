import { useState } from "react";
import { storeFaviconUrl, storeLabel } from "@/lib/catalogTitle";
import { cn } from "@/lib/utils";

/** Small storefront favicon (same Google source as Settings). */
export function StoreLogo({
  source,
  className,
}: {
  source: string;
  className?: string;
}) {
  const [failed, setFailed] = useState(false);
  const src = storeFaviconUrl(source);
  const label = storeLabel(source);
  if (!src || failed) {
    return (
      <div
        className={cn(
          "flex h-5 w-5 shrink-0 items-center justify-center rounded bg-fold text-[9px] font-semibold uppercase text-ink/70",
          className,
        )}
        aria-hidden
      >
        {label.slice(0, 2)}
      </div>
    );
  }
  return (
    <img
      src={src}
      alt=""
      className={cn(
        "h-5 w-5 shrink-0 rounded bg-white object-contain p-0.5",
        className,
      )}
      onError={() => setFailed(true)}
    />
  );
}
