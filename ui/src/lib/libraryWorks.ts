import type { FacetValuesPage, WorkFacet, WorkSummary, WorksPage, WorksQuery } from "@/lib/api";

/** The detail dialog's clickable metadata kinds (see `TitleDetailModal`). */
export type MetaSearchKind = "authors" | "narrators" | "series" | "genres";

/**
 * The works search a click on a detail's author, narrator, series or
 * genre runs.
 *
 * Authors, narrators and series use their field (`author:"…"`); genres
 * have no field in the works index, so a genre is searched as a phrase.
 *
 * @param kind - What was clicked.
 * @param value - Its text.
 * @returns A works query (`q`).
 */
export function metaSearchQuery(kind: MetaSearchKind, value: string): string {
  const phrase = `"${value.replace(/["\\]/g, " ").trim()}"`;
  switch (kind) {
    case "authors":
      return `author:${phrase}`;
    case "narrators":
      return `narrator:${phrase}`;
    case "series":
      return `series:${phrase}`;
    case "genres":
      return phrase;
  }
}

/** Works read from the first page on, and where the next page starts. */
export interface WorksRead {
  /** The works, in page order. */
  works: WorkSummary[];
  /** Cursor of the page after them; `null` when they are the last. */
  next: string | null;
}

/** Most works one request asks for (the daemon's own cap). */
export const MAX_WORKS_PAGE = 200;

/**
 * Reads pages of works until at least `wanted` are in hand or none are
 * left, starting at `cursor` (or the first page).
 *
 * A filtered search page can come back short and still carry a cursor, so
 * a read keeps going while pages answer with one; it stops after an empty
 * page that has no cursor.
 *
 * @param fetchPage - Reads one page (`fetchWorks`).
 * @param query - Filters and order, without `cursor` or `limit`.
 * @param wanted - How many works to read at least.
 * @param cursor - Where to start; omitted for the first page.
 * @param isCurrent - Checked between pages; a read for a stale query stops
 *   early, returning what it has.
 * @returns The works and the cursor after them.
 */
export async function readWorks(
  fetchPage: (query: WorksQuery) => Promise<WorksPage>,
  query: Omit<WorksQuery, "cursor" | "limit">,
  wanted: number,
  cursor?: string,
  isCurrent: () => boolean = () => true,
): Promise<WorksRead> {
  const works: WorkSummary[] = [];
  let next: string | null = cursor ?? null;
  let first = true;
  while (first || (next !== null && works.length < wanted && isCurrent())) {
    const page = await fetchPage({
      ...query,
      cursor: next ?? undefined,
      limit: Math.min(MAX_WORKS_PAGE, Math.max(1, wanted - works.length)),
    });
    works.push(...page.works);
    next = page.next_cursor ?? null;
    first = false;
  }
  return { works, next };
}

/** The library page's facet filter choices (`all` filters by none). */
export const FACET_OPTIONS: { value: WorkFacet | "all"; label: string }[] = [
  { value: "all", label: "No filter" },
  { value: "author", label: "Author" },
  { value: "series", label: "Series" },
  { value: "genre", label: "Genre" },
  { value: "narrator", label: "Narrator" },
  { value: "publisher", label: "Publisher" },
  { value: "source", label: "Store" },
  { value: "library_tag", label: "Library tags" },
  { value: "collection", label: "Collection" },
  { value: "tag", label: "My tags" },
  { value: "progress", label: "My progress" },
];

/** The select value standing for "in no series" (sent as an empty value). */
export const NO_SERIES = "__none__";

/** Most values {@link readFacetValues} reads for one menu. */
export const MAX_FACET_VALUES = 5000;

/**
 * The value choices of a facet filter's menu.
 *
 * Progress has fixed choices; series adds "No series" first; every other
 * facet lists the values the library's works have, read page by page (up
 * to {@link MAX_FACET_VALUES}).
 *
 * @param fetchPage - Reads one page of values (`fetchFacetValues`).
 * @param facet - The facet.
 * @returns Value and label pairs, in menu order.
 */
export async function readFacetValues(
  fetchPage: (facet: Exclude<WorkFacet, "progress">, cursor?: string) => Promise<FacetValuesPage>,
  facet: WorkFacet,
): Promise<{ value: string; label: string }[]> {
  if (facet === "progress") {
    return [
      { value: "finished", label: "Finished" },
      { value: "not_finished", label: "Not finished" },
    ];
  }
  const out: { value: string; label: string }[] =
    facet === "series" ? [{ value: NO_SERIES, label: "No series" }] : [];
  let cursor: string | undefined;
  let read = 0;
  do {
    const page = await fetchPage(facet, cursor);
    for (const v of page.values) {
      out.push({ value: v.value, label: `${v.value} (${v.works})` });
    }
    read += page.values.length;
    cursor = page.next_cursor ?? undefined;
  } while (cursor && read < MAX_FACET_VALUES);
  return out;
}

/**
 * The `facet` and `value` a works query sends for the menus' choice.
 *
 * @param facet - The chosen facet, or `all`.
 * @param value - The chosen value; `null` while none is chosen.
 * @returns The query fields; empty when no filter applies.
 */
export function facetQuery(
  facet: WorkFacet | "all",
  value: string | null,
): Pick<WorksQuery, "facet" | "value"> {
  if (facet === "all" || value === null) return {};
  return { facet, value: value === NO_SERIES ? "" : value };
}
