import { useEffect, useState, type ReactNode } from "react";
import { Download, X } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  coverUrl,
  type AcquireStatus,
  type BookRecord,
} from "@/lib/api";
import { formatDate, formatDuration } from "@/lib/libraryFilters";
import { cn } from "@/lib/utils";

const FALLBACK_COVER = "/bookclerk-mark.svg";

const statusStyles: Record<AcquireStatus, string> = {
  acquired: "bg-teal/15 text-teal",
  not_acquired: "bg-ink/10 text-ink/80",
  queued: "bg-parchment text-ink",
  downloading: "bg-teal/25 text-ink",
  error: "bg-brick/15 text-brick",
};

function absoluteCoverUrl(raw: string): string {
  const trimmed = raw.trim();
  if (trimmed.startsWith("//")) return `https:${trimmed}`;
  return trimmed;
}

function coverCandidates(book: BookRecord): string[] {
  const out: string[] = [coverUrl(book.uuid)];
  const remote = book.cover_url?.trim();
  if (remote) out.push(absoluteCoverUrl(remote));
  out.push(FALLBACK_COVER);
  return out;
}

function MetaRow({ label, value }: { label: string; value: ReactNode }) {
  if (value == null || value === "" || value === "—") return null;
  return (
    <div className="grid grid-cols-[7.5rem_1fr] gap-x-3 gap-y-1 text-sm sm:grid-cols-[9rem_1fr]">
      <dt className="text-ink/50">{label}</dt>
      <dd className="min-w-0 break-words text-ink">{value}</dd>
    </div>
  );
}

export function BookDetailModal({
  book,
  busy,
  onClose,
  onAcquire,
  showAcquire = true,
}: {
  book: BookRecord;
  busy: boolean;
  onClose: () => void;
  onAcquire: (book: BookRecord) => void;
  showAcquire?: boolean;
}) {
  const candidates = coverCandidates(book);
  const [coverIndex, setCoverIndex] = useState(0);
  const src = candidates[Math.min(coverIndex, candidates.length - 1)]!;
  const isFallback = src === FALLBACK_COVER;
  const series =
    book.series &&
    `${book.series}${book.series_index ? ` #${book.series_index}` : ""}`;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = prev;
    };
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-ink/40 p-0 backdrop-blur-[2px] sm:items-center sm:p-6"
      role="presentation"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="book-detail-title"
        className="animate-[detailIn_220ms_ease-out] flex max-h-[92vh] w-full max-w-2xl flex-col overflow-hidden rounded-t-xl bg-paper shadow-xl sm:rounded-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3 border-b border-ink/10 px-4 py-3 sm:px-5">
          <div className="min-w-0">
            <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
              Title details
            </p>
            <h2
              id="book-detail-title"
              className="mt-0.5 font-display text-xl font-bold leading-tight text-ink sm:text-2xl"
            >
              {book.title}
            </h2>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md p-1.5 text-ink/60 transition-colors hover:bg-ink/5 hover:text-ink"
            aria-label="Close"
          >
            <X className="h-5 w-5" />
          </button>
        </div>

        <div className="overflow-y-auto px-4 py-4 sm:px-5">
          <div className="flex flex-col gap-5 sm:flex-row">
            <div className="mx-auto h-44 w-44 shrink-0 overflow-hidden rounded-md bg-fold shadow-md sm:mx-0 sm:h-52 sm:w-52">
              <img
                src={src}
                alt={isFallback ? "" : `Cover for ${book.title}`}
                className={cn(
                  "h-full w-full",
                  isFallback ? "object-contain p-6 opacity-80" : "object-cover",
                )}
                onError={() => {
                  setCoverIndex((i) => Math.min(i + 1, candidates.length - 1));
                }}
              />
            </div>
            <div className="min-w-0 flex-1 space-y-3">
              <div className="flex flex-wrap gap-2">
                <Badge className={cn(statusStyles[book.acquire_status])}>
                  {book.acquire_status.replaceAll("_", " ")}
                </Badge>
                <Badge className="bg-ink/8 text-ink/70">{book.source}</Badge>
                {book.is_finished ? (
                  <Badge className="bg-teal/15 text-teal">Finished</Badge>
                ) : null}
                {book.is_abridged ? (
                  <Badge className="bg-parchment text-ink">Abridged</Badge>
                ) : null}
              </div>
              {book.subtitle ? (
                <p className="text-sm text-ink/70">{book.subtitle}</p>
              ) : null}
              <dl className="space-y-2">
                <MetaRow label="Authors" value={book.authors} />
                <MetaRow label="Narrators" value={book.narrators} />
                <MetaRow label="Series" value={series} />
                <MetaRow label="Publisher" value={book.publisher} />
                <MetaRow
                  label="Duration"
                  value={formatDuration(book.length_minutes)}
                />
                <MetaRow label="Published" value={formatDate(book.published_at)} />
                <MetaRow label="Purchased" value={formatDate(book.purchased_at)} />
                <MetaRow label="Genres" value={book.categories} />
                <MetaRow label="Tags" value={book.tags} />
                <MetaRow label="Kind" value={book.content_kind} />
                <MetaRow label="Marketplace" value={book.marketplace} />
                <MetaRow label="Account" value={book.account_id} />
                <MetaRow label="ASIN" value={book.asin} />
                <MetaRow label="ISBN" value={book.isbn} />
                <MetaRow label="Product ID" value={book.product_id} />
                <MetaRow label="Series ASIN" value={book.series_asin} />
                <MetaRow
                  label="Ratings"
                  value={
                    book.rating_overall != null
                      ? [
                          `overall ${book.rating_overall.toFixed(1)}`,
                          book.rating_performance != null
                            ? `perf ${book.rating_performance.toFixed(1)}`
                            : null,
                          book.rating_story != null
                            ? `story ${book.rating_story.toFixed(1)}`
                            : null,
                        ]
                          .filter(Boolean)
                          .join(" · ")
                      : null
                  }
                />
                <MetaRow label="Storage" value={book.storage_key} />
                <MetaRow label="PDF" value={book.pdf_status !== "not_acquired" ? book.pdf_status : null} />
                <MetaRow label="Added" value={formatDate(book.created_at)} />
                <MetaRow label="Updated" value={formatDate(book.updated_at)} />
                <MetaRow label="UUID" value={book.uuid} />
              </dl>
              {book.error_message ? (
                <p className="rounded-md bg-brick/10 px-3 py-2 text-sm text-brick">
                  {book.error_message}
                </p>
              ) : null}
            </div>
          </div>
        </div>

        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-ink/10 px-4 py-3 sm:px-5">
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
          {showAcquire && book.acquire_status !== "acquired" ? (
            <Button
              onClick={() => onAcquire(book)}
              disabled={busy || book.acquire_status === "downloading"}
            >
              <Download className="h-4 w-4" />
              Acquire
            </Button>
          ) : null}
        </div>
      </div>
      <style>{`
        @keyframes detailIn {
          from { opacity: 0; transform: translateY(12px) scale(0.985); }
          to { opacity: 1; transform: translateY(0) scale(1); }
        }
      `}</style>
    </div>
  );
}
