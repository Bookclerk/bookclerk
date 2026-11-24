import { useEffect, useRef, useState } from "react";
import { Headphones, Loader2, Pause, RotateCcw } from "lucide-react";
import { Button } from "@/components/ui/button";
import type { TitleMetaQuery } from "@/lib/api";
import {
  findSample,
  pauseBookFor,
  sampleSourceKey,
  storeSampleFallback,
  type FoundSample,
} from "@/lib/sample";
import { webPlayer } from "@/lib/webPlayer";

/**
 * Where the sample control is: ready to play, starting, playing, paused,
 * or failed (the audio could not be loaded; pressing again retries).
 */
type SampleStatus = "ready" | "loading" | "playing" | "paused" | "failed";

/**
 * The "Sample" control of a title's details: plays a short preview, cut
 * from the edition this library holds or offered by a storefront that
 * listed the title.
 *
 * The control is shown only once the daemon has said a sample can be
 * played here; a title with none (no store offers one, or nothing here can
 * cut one) shows no control at all.
 *
 * The preview plays in this control's own audio element, never the book
 * player, so it reports no listening position and does not count toward
 * recommendations. Starting it pauses a book that is playing.
 *
 * Key it by the open title's identity, so another title starts over. When
 * enrichment of the same title adds a held edition or an identifier, the
 * sample is looked up again, unless one is playing or paused.
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
  const sourceKey = sampleSourceKey(editionId, query);
  // The sample found for `sourceKey`; `null` while looking or when there
  // is none.
  const [found, setFound] = useState<{ key: string; sample: FoundSample } | null>(null);
  const [status, setStatus] = useState<SampleStatus>("ready");
  const audioRef = useRef<HTMLAudioElement | null>(null);
  // The sample the audio element holds, and whether the store fallback was
  // already tried for it.
  const currentRef = useRef<FoundSample | null>(null);
  const fellBackRef = useRef(false);
  const queryRef = useRef(query);
  const active = status === "playing" || status === "paused" || status === "loading";
  const activeRef = useRef(active);
  // The latest title and state, for the lookup and the audio handlers.
  // Declared before the lookup effect, so it runs first.
  useEffect(() => {
    queryRef.current = query;
    activeRef.current = active;
  });

  // Looks the sample up when the title's sources change; a sample that is
  // playing or paused carries on.
  useEffect(() => {
    if (activeRef.current) return;
    let live = true;
    findSample(editionId, queryRef.current)
      .then((sample) => {
        if (!live) return;
        setFound(sample ? { key: sourceKey, sample } : null);
        setStatus("ready");
      })
      .catch(() => {
        // Offline or a server error: offer nothing rather than a control
        // that cannot play.
        if (live) setFound(null);
      });
    return () => {
      live = false;
    };
  }, [sourceKey, editionId]);

  // Unmounting (closing the dialog, or another title: the parent keys this
  // control by title) stops the sample.
  useEffect(() => {
    const audio = audioRef.current;
    return () => {
      if (audio) {
        audio.pause();
        audio.removeAttribute("src");
        audio.load();
      }
    };
  }, []);

  /** Loads `sample` into the audio element and starts it. */
  const start = async (sample: FoundSample) => {
    const audio = audioRef.current;
    if (!audio) return;
    currentRef.current = sample;
    setStatus("loading");
    // The book may have resumed (a media key) meanwhile; pause it again so
    // the two never play over each other.
    pauseBookFor(webPlayer());
    audio.src = sample.url;
    try {
      await audio.play();
    } catch {
      // A source that cannot play raises `error` too, which is handled
      // there; a blocked autoplay leaves the control paused.
      if (!audio.error) setStatus("paused");
    }
  };

  /**
   * The audio failed: a held edition's sample falls back once to a
   * store's; anything else settles on "failed", which retries.
   */
  const onAudioError = async () => {
    const current = currentRef.current;
    if (!current) return;
    if (current.kind === "edition" && !fellBackRef.current) {
      fellBackRef.current = true;
      try {
        const store = await storeSampleFallback(queryRef.current);
        if (store && currentRef.current === current) {
          void start(store);
          return;
        }
      } catch {
        // Settles on "failed" below.
      }
    }
    if (currentRef.current === current) setStatus("failed");
  };

  const sample = found && found.key === sourceKey ? found.sample : null;
  // Nothing to offer, and nothing playing: no control.
  const shown = sample !== null || active;

  const onClick = () => {
    const audio = audioRef.current;
    if (!audio) return;
    if (status === "playing") {
      audio.pause();
      return;
    }
    pauseBookFor(webPlayer());
    if (status === "paused" && currentRef.current) {
      void audio.play().catch(() => setStatus("paused"));
      return;
    }
    if (!sample) return;
    fellBackRef.current = false;
    void start(sample);
  };

  const label =
    status === "playing"
      ? "Pause sample"
      : status === "loading"
        ? "Loading sample…"
        : status === "failed"
          ? "Retry sample"
          : "Sample";

  return (
    <>
      {shown ? (
        <Button
          variant="secondary"
          onClick={onClick}
          disabled={status === "loading"}
          aria-pressed={status === "playing"}
          title={
            status === "failed"
              ? "The sample could not be loaded; press to try again"
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
      ) : null}
      <audio
        ref={audioRef}
        preload="none"
        className="hidden"
        onPlaying={() => setStatus("playing")}
        onPause={() => setStatus((s) => (s === "playing" ? "paused" : s))}
        onEnded={() => setStatus("paused")}
        onError={() => void onAudioError()}
      />
    </>
  );
}
