import { cn } from "@/lib/utils";

/**
 * Animated waveform loading indicator.
 *
 * @param props - Optional className and size (`xs` fits an avatar badge).
 */
export function WaveformThrobber({
  className,
  size = "md",
}: {
  className?: string;
  size?: "xs" | "sm" | "md" | "lg";
}) {
  const dim =
    size === "xs"
      ? "h-2.5 w-3"
      : size === "sm"
        ? "h-3.5 w-4"
        : size === "lg"
          ? "h-11 w-14"
          : "h-9 w-11";

  return (
    <svg
      viewBox="0 0 79 114"
      className={cn("text-brick", dim, className)}
      aria-hidden
      focusable="false"
    >
      {/*
        Proportions from ui/public/bookclerk-logo.svg (three #C84A34 bars).
        Bars grow from their vertical centers like a listening equalizer.
      */}
      <rect
        x="0"
        y="23"
        width="17"
        height="68"
        rx="8.5"
        fill="currentColor"
        className="waveform-bar"
        style={{ animationDelay: "0ms" }}
      />
      <rect
        x="31"
        y="0"
        width="17"
        height="114"
        rx="8.5"
        fill="currentColor"
        className="waveform-bar"
        style={{ animationDelay: "140ms" }}
      />
      <rect
        x="62"
        y="26"
        width="17"
        height="62"
        rx="8.5"
        fill="currentColor"
        className="waveform-bar"
        style={{ animationDelay: "280ms" }}
      />
    </svg>
  );
}
