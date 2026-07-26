import * as React from "react";
import { cn } from "@/lib/utils";

export function Input({
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement>) {
  return (
    <input
      className={cn(
        "w-full rounded-md border border-ink/15 bg-white/80 px-3 py-2 text-sm text-ink shadow-sm placeholder:text-ink/40 focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30",
        className,
      )}
      {...props}
    />
  );
}
