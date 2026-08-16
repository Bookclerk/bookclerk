import * as React from "react";
import { cn } from "@/lib/utils";

/**
 * Styled text input matching Bookclerk form controls.
 *
 * Set `aria-invalid` for a brick border and focus ring on invalid values.
 *
 * @param props - Standard input attributes.
 */
export function Input({
  className,
  ...props
}: React.InputHTMLAttributes<HTMLInputElement>) {
  const invalid =
    props["aria-invalid"] === true || props["aria-invalid"] === "true";
  return (
    <input
      className={cn(
        "w-full rounded-md border border-ink/15 bg-card-strong px-3 py-2 text-sm text-ink shadow-sm placeholder:text-ink/40 focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30",
        invalid &&
          "border-brick focus:border-brick focus:ring-brick/30",
        className,
      )}
      {...props}
    />
  );
}
