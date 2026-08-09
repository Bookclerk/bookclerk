import { Star } from "lucide-react";
import { cn } from "@/lib/utils";

type StarRatingProps = {
  /** Rating on a 0–`max` scale (fractional values get a clipped fill). */
  value: number;
  max?: number;
  className?: string;
  /** Size classes for each star glyph (width + height). */
  starClassName?: string;
};

/**
 * Five-star row with Lucide stars: solid teal fill, outlined empties, and a
 * width-clipped partial fill for fractional overall ratings (e.g. 4.8).
 */
export function StarRating({
  value,
  max = 5,
  className,
  starClassName = "h-3.5 w-3.5",
}: StarRatingProps) {
  const clamped = Math.max(0, Math.min(max, Number.isFinite(value) ? value : 0));
  return (
    <span
      className={cn("inline-flex items-center gap-0.5", className)}
      aria-label={`${clamped.toFixed(1)} out of ${max} stars`}
      title={`${clamped.toFixed(1)}/${max}`}
    >
      {Array.from({ length: max }, (_, i) => {
        const fill = Math.max(0, Math.min(1, clamped - i));
        return (
          <span
            key={i}
            className={cn("relative inline-block shrink-0", starClassName)}
            aria-hidden
          >
            <Star
              className={cn(
                "absolute inset-0 fill-transparent text-ink/55 stroke-[2.25]",
                starClassName,
              )}
            />
            {fill > 0 ? (
              <span
                className="absolute inset-y-0 left-0 overflow-hidden"
                style={{ width: `${fill * 100}%` }}
              >
                <Star className={cn("fill-teal text-teal", starClassName)} />
              </span>
            ) : null}
          </span>
        );
      })}
    </span>
  );
}
