import { useEffect, useRef, useState } from "react";
import { coverSrc, type CoverSize } from "@/lib/api";
import { cn } from "@/lib/utils";

const FALLBACK_COVER = "/bookclerk-mark.svg";

/**
 * Cover art from same-origin sources only, over a placeholder.
 *
 * The placeholder shows until an image loads, then the cover fades in. Each
 * candidate is tried in order and the next one replaces it on error; when
 * none loads, the placeholder stays. Images load lazily, and one still
 * loading when the cover leaves the page is cancelled.
 *
 * @param props - `path` is a `cover_path` from the API, loaded at `size`;
 *   `sources` are other same-origin URLs tried first (a library sidecar);
 *   `className` sizes the frame.
 */
export function CoverThumb({
  path,
  size = 128,
  sources = [],
  className = "h-12 w-12",
}: {
  path?: string | null;
  size?: CoverSize;
  sources?: string[];
  className?: string;
}) {
  const remote = coverSrc(path, size);
  const candidates = remote ? [...sources, remote] : sources;
  const key = candidates.join("\n");
  const [state, setState] = useState({ key, index: 0, loaded: false });
  // A new set of candidates starts over from the first one.
  const current = state.key === key ? state : { key, index: 0, loaded: false };
  const src = candidates[current.index] ?? null;
  const frame = cn(
    "relative block shrink-0 overflow-hidden rounded-md bg-ink/5 shadow-sm",
    className,
  );
  return (
    <span className={frame} aria-hidden>
      {current.loaded ? null : (
        <img
          src={FALLBACK_COVER}
          alt=""
          className="absolute inset-0 h-full w-full object-contain p-[18%] opacity-70"
        />
      )}
      {src ? (
        <CoverLayer
          key={src}
          src={src}
          onLoad={() => setState({ ...current, loaded: true })}
          onError={() => setState({ ...current, index: current.index + 1, loaded: false })}
        />
      ) : null}
    </span>
  );
}

/**
 * One cover image that fades in once loaded and cancels its download when it
 * unmounts first.
 *
 * @param props - Image URL and load/error callbacks.
 */
function CoverLayer({
  src,
  onLoad,
  onError,
}: {
  src: string;
  onLoad: () => void;
  onError: () => void;
}) {
  const ref = useRef<HTMLImageElement>(null);
  const [shown, setShown] = useState(false);
  useEffect(() => {
    const img = ref.current;
    // A remount (StrictMode runs effects twice) restores what the cleanup
    // below removed; React won't, since the `src` prop didn't change.
    if (img && img.getAttribute("src") !== src) img.setAttribute("src", src);
    return () => {
      // Dropping `src` aborts a request the browser has not finished.
      if (img && !img.complete) img.removeAttribute("src");
    };
  }, [src]);
  return (
    <img
      ref={ref}
      src={src}
      alt=""
      loading="lazy"
      decoding="async"
      className={cn(
        "absolute inset-0 h-full w-full object-cover motion-safe:transition-opacity motion-safe:duration-300",
        shown ? "opacity-100" : "opacity-0",
      )}
      onLoad={() => {
        setShown(true);
        onLoad();
      }}
      onError={onError}
    />
  );
}
