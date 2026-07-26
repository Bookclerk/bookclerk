import * as React from "react";
import { cn } from "@/lib/utils";

type Variant = "primary" | "secondary" | "ghost" | "danger";

const variants: Record<Variant, string> = {
  primary:
    "bg-ink text-paper hover:bg-ink-soft shadow-sm disabled:opacity-50",
  secondary:
    "bg-teal/15 text-ink border border-teal/30 hover:bg-teal/25 disabled:opacity-50",
  ghost: "bg-transparent text-ink hover:bg-ink/5 disabled:opacity-50",
  danger:
    "bg-brick/10 text-brick border border-brick/30 hover:bg-brick/20 disabled:opacity-50",
};

export function Button({
  className,
  variant = "primary",
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      className={cn(
        "inline-flex items-center justify-center gap-2 rounded-md px-3.5 py-2 text-sm font-semibold transition-colors focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-teal",
        variants[variant],
        className,
      )}
      {...props}
    />
  );
}
