import { useState } from "react";
import { Download } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  coverUrl,
  type AcquireStatus,
  type BookRecord,
} from "@/lib/api";
import { cn } from "@/lib/utils";

const statusStyles: Record<AcquireStatus, string> = {
  acquired: "bg-teal/15 text-teal",
  not_acquired: "bg-ink/10 text-ink/80",
  queued: "bg-parchment text-ink",
  downloading: "bg-teal/25 text-ink",
  error: "bg-brick/15 text-brick",
};

export function BookRow({
  book,
  onAcquire,
  onOpen,
  busy,
  showAcquire = true,
}: {
  book: BookRecord;
  onAcquire: (book: BookRecord) => void;
  onOpen?: (book: BookRecord) => void;
  busy: boolean;
  showAcquire?: boolean;
}) {
  const [coverFailed, setCoverFailed] = useState(false);
  const meta = [book.authors, book.narrators ? `narr. ${book.narrators}` : null]
    .filter(Boolean)
    .join(" · ");
  const series =
    book.series &&
    `${book.series}${book.series_index ? ` #${book.series_index}` : ""}`;

  return (
    <div className="group grid grid-cols-[56px_1fr_auto] items-center gap-3 border-b border-ink/10 px-3 py-2.5 transition-colors hover:bg-white/50 sm:grid-cols-[64px_1fr_auto] sm:gap-4 sm:px-4">
      <button
        type="button"
        className="relative h-14 w-14 overflow-hidden rounded-sm bg-fold shadow-sm sm:h-16 sm:w-16"
        onClick={() => onOpen?.(book)}
        aria-label={`Open details for ${book.title}`}
      >
        {coverFailed ? (
          <img
            src="/bookclerk-mark.svg"
            alt=""
            className="h-full w-full object-contain p-2 opacity-80"
          />
        ) : (
          <img
            src={coverUrl(book.uuid)}
            alt=""
            className="h-full w-full object-cover"
            onError={() => setCoverFailed(true)}
          />
        )}
      </button>
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="truncate font-display text-base font-semibold leading-tight text-ink sm:text-lg">
            {onOpen ? (
              <button
                type="button"
                className="text-left hover:underline"
                onClick={() => onOpen(book)}
              >
                {book.title}
              </button>
            ) : (
              book.title
            )}
          </h2>
          <Badge className={cn(statusStyles[book.acquire_status])}>
            {book.acquire_status.replaceAll("_", " ")}
          </Badge>
          <Badge className="bg-ink/8 text-ink/70">{book.source}</Badge>
        </div>
        {meta ? (
          <p className="mt-0.5 truncate text-sm text-ink/70">{meta}</p>
        ) : null}
        <p className="mt-0.5 truncate text-xs text-ink/50">
          {[series, book.asin || book.isbn || book.product_id]
            .filter(Boolean)
            .join(" · ")}
        </p>
        {book.error_message ? (
          <p className="mt-1 truncate text-xs text-brick">{book.error_message}</p>
        ) : null}
      </div>
      <div className="flex shrink-0 items-center">
        {showAcquire && book.acquire_status !== "acquired" ? (
          <Button
            variant="secondary"
            className="px-2.5 py-1.5"
            disabled={busy || book.acquire_status === "downloading"}
            onClick={() => onAcquire(book)}
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
