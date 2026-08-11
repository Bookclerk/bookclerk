import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

/**
 * Small uppercase status/label chip.
 *
 * @param props - Optional className and children.
 */
export function Badge({
  className,
  children,
}: {
  className?: string;
  children: ReactNode;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-[11px] font-semibold uppercase tracking-wide",
        className,
      )}
    >
      {children}
    </span>
  );
}
