import { useEffect, useState } from "react";
import { CoverThumb } from "@/components/CoverThumb";
import { editionCoverUrl, fetchSimilarWorks, type WorkSummary } from "@/lib/api";

/** Most works the row shows. */
const SHOWN = 8;

/** Props of {@link SimilarWorks}. */
export interface SimilarWorksProps {
  /** The work to find others like. */
  workId: string;
  /** Opens one of the listed works. */
  onOpen: (work: WorkSummary) => void;
}

/**
 * "More like this": the library's works closest in meaning to one work,
 * most alike first. Renders nothing while it loads, when the library has
 * no embedding model, or when nothing is alike enough to list.
 */
export function SimilarWorks({ workId, onOpen }: SimilarWorksProps) {
  // The works read, with the work they were read for.
  const [read, setRead] = useState<{ workId: string; works: WorkSummary[] } | null>(null);

  useEffect(() => {
    let current = true;
    fetchSimilarWorks(workId, SHOWN)
      .then((res) => {
        if (current) setRead({ workId, works: res.state === "ready" ? res.works : [] });
      })
      .catch(() => {
        // An extra of the dialog: without it the dialog is complete.
        if (current) setRead({ workId, works: [] });
      });
    return () => {
      current = false;
    };
  }, [workId]);

  const works = read?.workId === workId ? read.works : [];
  if (works.length === 0) return null;
  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="More like this">
      <h3 className="font-display text-sm font-semibold text-ink">More like this</h3>
      <ul className="flex gap-3 overflow-x-auto pb-1">
        {works.map((work) => (
          <li key={work.id} className="w-24 shrink-0">
            <button
              type="button"
              className="group flex w-full flex-col gap-1 rounded-md text-left focus:outline-none focus-visible:ring-2 focus-visible:ring-teal/50"
              onClick={() => onOpen(work)}
            >
              <CoverThumb
                path={work.cover_path}
                sources={
                  work.cover_edition_id && work.acquire_status === "acquired"
                    ? [editionCoverUrl(work.cover_edition_id)]
                    : []
                }
                sourcesLast
                className="h-24 w-24"
              />
              <span className="line-clamp-2 text-xs font-medium leading-snug text-ink group-hover:underline">
                {work.title}
              </span>
              {work.authors ? (
                <span className="truncate text-xs text-ink/70">{work.authors}</span>
              ) : null}
            </button>
          </li>
        ))}
      </ul>
    </section>
  );
}
