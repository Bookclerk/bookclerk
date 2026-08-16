import logoSvg from "../../public/bookclerk-logo.svg?raw";
import { cn } from "@/lib/utils";

const LOGO_INNER = logoSvg
  .replace(/^\uFEFF/, "")
  .replace(/<\?xml[^?]*\?>\s*/i, "")
  .trim();

/**
 * Horizontal Bookclerk lockup. Inlined so dark mode can recolor the wordmark
 * to parchment (`currentColor` / `text-ink`) without touching the mark.
 *
 * @param props - Size and spacing classes, applied to the root SVG.
 */
export function BookclerkLogo({ className }: { className?: string }) {
  return (
    <span
      className="contents"
      dangerouslySetInnerHTML={{
        __html: LOGO_INNER.replace(
          "<svg ",
          `<svg class="${cn("bookclerk-logo text-ink", className)}" `,
        ),
      }}
    />
  );
}
