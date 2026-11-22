import { useEffect, useRef, useState } from "react";
import { Headphones, Loader2, Pause, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { TitleMetaQuery } from "@/lib/api";
import { pauseBookFor, sampleSources } from "@/lib/sample";
import { webPlayer } from "@/lib/webPlayer";

/**
 * Where the sample control is: nothing tried yet, finding audio, playing,
 * paused, none found, or the store lookup failed (pressing again retries).
 */
type SampleStatus = "idle" | "loading" | "playing" | "paused" | "none" | "failed";

/**
 * The "Sample" control of a title's details: plays a short preview, cut
 * from the edition this library holds or offered by a storefront that
 * listed the title.
 *
 * The preview plays in this control's own audio element, never the book
 * player, so it reports no listening position and does not count toward
 * recommendations. Starting it pauses a book that is playing.
 *
 * Key it by title so another title starts over. The query is read when
 * the control is pressed, so an ASIN or ISBN that title-meta adds later is
 * used without restarting a sample.
 *
 * @param props - `editionId` is the held edition, when there is one;
 *   `query` is the title as title-meta sees it.
 */
export function SampleButton({
  editionId,
  query,
}: {
  editionId?: string | null;
  query: TitleMetaQuery;
}) {
  const [status, setStatus] = useState<SampleStatus>("idle");
  const audioRef = useRef<HTMLAudioElement | null>(null);
  const sourcesRef = useRef<AsyncGenerator<string, void, undefined> | null>(null);
  // Unmounting (closing the dialog, or another title: the parent keys this
  // control by title) stops the sample.
  useEffect(() => {
    const audio = audioRef.current;
    return () => {
      sourcesRef.current = null;
      if (audio) {
        audio.pause();
        audio.removeAttribute("src");
        audio.load();
      }
    };
  }, []);

  /**
   * Moves to the next source, settling on "none" when they ran out or on
   * "failed" when the store lookup did.
   */
  const playNext = async () => {
    const audio = audioRef.current;
    const sources = sourcesRef.current;
    if (!audio || !sources) return;
    setStatus("loading");
    let next: IteratorResult<string, void>;
    try {
      next = await sources.next();
    } catch {
      if (sourcesRef.current !== sources) return;
      sourcesRef.current = null;
      setStatus("failed");
      return;
    }
    if (sourcesRef.current !== sources) return;
    if (next.done) {
      setStatus("none");
      return;
    }
    // The book may have resumed (a media key) while the lookup ran; pause
    // it again so the two never play over each other.
    pauseBookFor(webPlayer());
    audio.src = next.value;
    try {
      await audio.play();
    } catch {
      // A source that cannot play raises `error` too, which moves on; a
      // blocked autoplay leaves the control paused.
      if (!audio.error) setStatus("paused");
    }
  };

  const onClick = () => {
    const audio = audioRef.current;
    if (!audio) return;
    if (status === "playing") {
      audio.pause();
      return;
    }
    pauseBookFor(webPlayer());
    if (status === "paused") {
      void audio.play().catch(() => setStatus("paused"));
      return;
    }
    sourcesRef.current = sampleSources(editionId, query);
    void playNext();
  };

  const label =
    status === "playing"
      ? "Pause sample"
      : status === "loading"
        ? "Finding sample…"
        : status === "none"
          ? "No sample"
          : status === "failed"
            ? "Retry sample"
            : "Sample";

  return (
    <>
      <Button
        variant="secondary"
        onClick={onClick}
        disabled={status === "loading" || status === "none"}
        aria-pressed={status === "playing"}
        title={
          status === "failed"
            ? "The stores could not be asked; press to try again"
            : "A short preview; it does not count as listening"
        }
      >
        {status === "playing" ? (
          <Pause className="h-4 w-4" />
        ) : status === "loading" ? (
          <Loader2 className="h-4 w-4 animate-spin" />
        ) : status === "failed" ? (
          <RotateCcw className="h-4 w-4" />
        ) : (
          <Headphones className="h-4 w-4" />
        )}
        {label}
      </Button>
      <audio
        ref={audioRef}
        preload="none"
        className="hidden"
        onPlaying={() => setStatus("playing")}
        onPause={() => setStatus((s) => (s === "playing" ? "paused" : s))}
        onEnded={() => setStatus("paused")}
        onError={() => {
          if (sourcesRef.current) void playNext();
        }}
      />
    </>
  );
}
