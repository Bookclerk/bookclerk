import { useEffect, useRef, useState } from "react";
import { coverSrc, type CoverSize } from "@/lib/api";
import { coverRetryDelay } from "@/lib/coverRetry";
import { cn } from "@/lib/utils";

const FALLBACK_COVER = "/bookclerk-mark.svg";

/**
 * Cover art from same-origin sources only, over a placeholder.
 *
 * The placeholder shows until an image loads, then the cover fades in. Each
 * candidate is tried in order and the next one replaces it on error; when
 * none loads, the placeholder stays and the candidates are tried again later
 * (see {@link coverRetryDelay}), once the cover is on screen. Images load
 * lazily, and one still loading when the cover leaves the page is cancelled.
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
  const [state, setState] = useState<CoverState>(() => freshState(key));
  // A new set of candidates starts over from the first one.
  const current = state.key === key ? state : freshState(key);
  const src = candidates[current.index] ?? null;
  const exhausted = candidates.length > 0 && src === null;
  const frameRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!exhausted) return;
    const delay = coverRetryDelay(current.retry);
    const frame = frameRef.current;
    if (delay === null || !frame) return;
    let due = false;
    let visible = false;
    const retry = () =>
      setState({
        key,
        index: 0,
        loaded: false,
        retry: current.retry + 1,
        round: current.round + 1,
      });
    const timer = window.setTimeout(() => {
      due = true;
      if (visible) retry();
    }, delay);
    // Off screen, wait until it scrolls back in rather than load it unseen.
    const observer =
      typeof IntersectionObserver === "undefined"
        ? null
        : new IntersectionObserver((entries) => {
            visible = entries.some((entry) => entry.isIntersecting);
            if (visible && due) retry();
          });
    if (observer) observer.observe(frame);
    else visible = true;
    return () => {
      window.clearTimeout(timer);
      observer?.disconnect();
    };
  }, [exhausted, key, current.retry, current.round]);

  const frame = cn(
    "relative block shrink-0 overflow-hidden rounded-md bg-ink/5 shadow-sm",
    className,
  );
  return (
    <span ref={frameRef} className={frame} aria-hidden>
      {current.loaded ? null : (
        <img
          src={FALLBACK_COVER}
          alt=""
          className="absolute inset-0 h-full w-full object-contain p-[18%] opacity-70"
        />
      )}
      {src ? (
        <CoverLayer
          // A new round remounts the image so the browser asks again.
          key={`${current.round}\n${src}`}
          src={src}
          onLoad={() => setState({ ...current, loaded: true, retry: 0 })}
          onError={() => setState({ ...current, index: current.index + 1, loaded: false })}
        />
      ) : null}
    </span>
  );
}

/** Where a {@link CoverThumb} is in trying its candidates. */
type CoverState = {
  /** The candidates these fields describe, joined. */
  key: string;
  /** The candidate being tried; past the end when all failed. */
  index: number;
  /** Whether a candidate has loaded. */
  loaded: boolean;
  /** Retry rounds made since the cover last loaded. */
  retry: number;
  /** Rounds started in all, so each one remounts the image. */
  round: number;
};

/**
 * The state for a new set of candidates.
 *
 * @param key - The candidates, joined.
 * @returns State that tries the first candidate.
 */
function freshState(key: string): CoverState {
  return { key, index: 0, loaded: false, retry: 0, round: 0 };
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
