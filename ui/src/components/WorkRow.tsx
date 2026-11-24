import type { ReactNode } from "react";
import { CoverThumb } from "@/components/CoverThumb";
import { Download, Play } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { editionCoverUrl, type AcquireStatus, type MediaKind, type WorkSummary } from "@/lib/api";
import { rowOffersAcquire, rowOffersPlay } from "@/lib/playTarget";
import { cn } from "@/lib/utils";

const statusStyles: Record<AcquireStatus, string> = {
  acquired: "bg-teal/15 text-teal-text",
  not_acquired: "bg-ink/10 text-ink/80",
  queued: "bg-parchment text-ink",
  downloading: "bg-teal/25 text-ink",
  error: "bg-brick/15 text-brick-text",
};

const mediaLabels: Record<MediaKind, string> = {
  audio: "Audio",
  ebook: "Ebook",
  print: "Print",
};

/**
 * One library row: a work's cover, title, people, and its play or acquire
 * button.
 *
 * @param props - Work, handlers, busy flag, and acquire visibility. With
 *   `onPlay`, a work whose audio is acquired shows a play button; `note`
 *   is a last line under the people (such as the other libraries that
 *   hold the work too).
 */
export function WorkRow({
  work,
  onAcquire,
  onOpen,
  onPlay,
  busy,
  showAcquire = true,
  note,
}: {
  work: WorkSummary;
  onAcquire: (work: WorkSummary) => void;
  onOpen?: (work: WorkSummary) => void;
  onPlay?: (work: WorkSummary) => void;
  busy: boolean;
  showAcquire?: boolean;
  note?: ReactNode;
}) {
  const series = work.series
    ? `${work.series}${work.series_position ? ` #${work.series_position}` : ""}`
    : null;
  const editions = work.edition_count > 1 ? `${work.edition_count} editions` : null;

  return (
    <div className="group grid grid-cols-[56px_1fr_auto] items-center gap-3 border-b border-ink/10 px-3 py-2.5 transition-colors hover:bg-card-mid sm:grid-cols-[64px_1fr_auto] sm:gap-4 sm:px-4">
      <button
        type="button"
        className="relative h-14 w-14 overflow-hidden rounded-md bg-fold shadow-sm sm:h-16 sm:w-16"
        onClick={() => onOpen?.(work)}
        aria-label={`Open details for ${work.title}`}
      >
        {/* The storefront's cover through the daemon's cover cache first
            (already thumbnail-sized and cached by the browser), then the
            local sidecar of the cover edition (only acquired editions have
            one; full size), then the Bookclerk mark. */}
        <CoverThumb
          path={work.cover_path}
          sources={
            work.cover_edition_id && work.acquire_status === "acquired"
              ? [editionCoverUrl(work.cover_edition_id)]
              : []
          }
          sourcesLast
          className="h-full w-full rounded-none bg-transparent shadow-none"
        />
      </button>
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="truncate font-display text-base font-semibold leading-tight text-ink sm:text-lg">
            {onOpen ? (
              <button
                type="button"
                className="text-left hover:underline"
                onClick={() => onOpen(work)}
              >
                {work.title}
              </button>
            ) : (
              work.title
            )}
          </h2>
          <Badge className={cn(statusStyles[work.acquire_status])}>
            {work.acquire_status.replaceAll("_", " ")}
          </Badge>
          {work.kind === "podcast" ? <Badge className="bg-ink/8 text-ink/70">Podcast</Badge> : null}
          {work.media_kinds.map((kind) => (
            <Badge key={kind} className="bg-ink/8 text-ink/70">
              {mediaLabels[kind]}
            </Badge>
          ))}
        </div>
        {work.authors ? (
          <p className="mt-0.5 truncate text-sm text-ink/70">{work.authors}</p>
        ) : null}
        {series || editions ? (
          <p className="mt-0.5 truncate text-xs text-ink/70">
            {[series, editions].filter(Boolean).join(" · ")}
          </p>
        ) : null}
        {note}
      </div>
      <div className="flex shrink-0 items-center">
        {onPlay && rowOffersPlay(work) ? (
          <Button
            variant="secondary"
            className="px-2.5 py-1.5"
            onClick={() => onPlay(work)}
            title="Play"
            aria-label={`Play ${work.title}`}
          >
            <Play className="h-4 w-4" />
            <span className="hidden sm:inline">Play</span>
          </Button>
        ) : null}
        {showAcquire && rowOffersAcquire(work) ? (
          <Button
            variant="secondary"
            className="px-2.5 py-1.5"
            disabled={busy || work.acquire_status === "downloading"}
            onClick={() => onAcquire(work)}
            title="Acquire"
          >
            <Download className="h-4 w-4" />
            <span className="hidden sm:inline">Acquire</span>
          </Button>
        ) : null}
      </div>
    </div>
  );
}
