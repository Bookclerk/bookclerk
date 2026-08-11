import type { BookRecord } from "@/lib/api";

/**
 * AudioBookshelf-style library filter categories.
 */
export type FilterKind =
  | "all"
  | "authors"
  | "series"
  | "genres"
  | "tags"
  | "narrators"
  | "publishers"
  | "sources"
  | "progress"
  | "status";

/**
 * Client-side library table sort keys.
 */
export type SortKey =
  | "title"
  | "author"
  | "series"
  | "purchased"
  | "length"
  | "added";

/**
 * Active library filter kind plus selected value.
 */
export interface LibraryFilter {
  kind: FilterKind;
  /** Selected value within the kind (`all` / empty = no value filter). */
  value: string;
}

/**
 * UI labels for {@link FilterKind} select options.
 */
export const FILTER_KINDS: { value: FilterKind; label: string }[] = [
  { value: "all", label: "All" },
  { value: "authors", label: "Authors" },
  { value: "series", label: "Series" },
  { value: "genres", label: "Genres" },
  { value: "tags", label: "Tags" },
  { value: "narrators", label: "Narrators" },
  { value: "publishers", label: "Publishers" },
  { value: "sources", label: "Sources" },
  { value: "progress", label: "Progress" },
  { value: "status", label: "Acquire status" },
];

/**
 * UI labels for {@link SortKey} select options.
 */
export const SORT_OPTIONS: { value: SortKey; label: string }[] = [
  { value: "title", label: "Title" },
  { value: "author", label: "Author" },
  { value: "series", label: "Series" },
  { value: "purchased", label: "Purchase date" },
  { value: "added", label: "Date added" },
  { value: "length", label: "Duration" },
];

const PROGRESS_OPTIONS = [
  { value: "finished", label: "Finished" },
  { value: "not_finished", label: "Not finished" },
];

function splitList(raw: string | null | undefined): string[] {
  if (!raw?.trim()) return [];
  return raw
    .split(/[,;|]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

function uniqueSorted(values: Iterable<string>): string[] {
  return [...new Set(values)].sort((a, b) =>
    a.localeCompare(b, undefined, { sensitivity: "base" }),
  );
}

/**
 * Builds distinct filter value options for a kind from the loaded books.
 *
 * @param books - Library rows to mine for facet values.
 * @param kind - Filter category.
 * @returns Value/label pairs (empty for `all`).
 */
export function filterValuesForKind(
  books: BookRecord[],
  kind: FilterKind,
): { value: string; label: string }[] {
  switch (kind) {
    case "all":
      return [];
    case "authors":
      return uniqueSorted(books.flatMap((b) => splitList(b.authors))).map(
        (v) => ({ value: v, label: v }),
      );
    case "series": {
      const named = uniqueSorted(
        books.map((b) => b.series?.trim()).filter(Boolean) as string[],
      ).map((v) => ({ value: v, label: v }));
      const hasNone = books.some((b) => !b.series?.trim());
      return hasNone
        ? [{ value: "__none__", label: "No series" }, ...named]
        : named;
    }
    case "genres":
      return uniqueSorted(books.flatMap((b) => splitList(b.categories))).map(
        (v) => ({ value: v, label: v }),
      );
    case "tags":
      return uniqueSorted(
        books.flatMap((b) =>
          (b.tags ?? "")
            .split(/\s+/)
            .map((t) => t.trim())
            .filter(Boolean),
        ),
      ).map((v) => ({ value: v, label: v }));
    case "narrators":
      return uniqueSorted(books.flatMap((b) => splitList(b.narrators))).map(
        (v) => ({ value: v, label: v }),
      );
    case "publishers":
      return uniqueSorted(
        books.map((b) => b.publisher?.trim()).filter(Boolean) as string[],
      ).map((v) => ({ value: v, label: v }));
    case "sources":
      return uniqueSorted(books.map((b) => b.source)).map((v) => ({
        value: v,
        label: v,
      }));
    case "progress":
      return PROGRESS_OPTIONS;
    case "status":
      return [
        { value: "not_acquired", label: "Not acquired" },
        { value: "queued", label: "Queued" },
        { value: "downloading", label: "Downloading" },
        { value: "acquired", label: "Acquired" },
        { value: "error", label: "Error" },
      ];
  }
}

/**
 * Returns whether a book matches the active library filter.
 *
 * @param book - Library row.
 * @param filter - Kind + value (`all` / empty value always matches).
 * @returns True when the book should remain visible.
 */
export function bookMatchesFilter(
  book: BookRecord,
  filter: LibraryFilter,
): boolean {
  if (filter.kind === "all" || !filter.value) return true;
  switch (filter.kind) {
    case "authors":
      return splitList(book.authors).some(
        (a) => a.toLowerCase() === filter.value.toLowerCase(),
      );
    case "series":
      if (filter.value === "__none__") return !book.series?.trim();
      return (book.series ?? "").trim().toLowerCase() === filter.value.toLowerCase();
    case "genres":
      return splitList(book.categories).some(
        (g) => g.toLowerCase() === filter.value.toLowerCase(),
      );
    case "tags":
      return (book.tags ?? "")
        .split(/\s+/)
        .some((t) => t.toLowerCase() === filter.value.toLowerCase());
    case "narrators":
      return splitList(book.narrators).some(
        (n) => n.toLowerCase() === filter.value.toLowerCase(),
      );
    case "publishers":
      return (book.publisher ?? "").trim().toLowerCase() === filter.value.toLowerCase();
    case "sources":
      return book.source.toLowerCase() === filter.value.toLowerCase();
    case "progress":
      if (filter.value === "finished") return book.is_finished;
      if (filter.value === "not_finished") return !book.is_finished;
      return true;
    case "status":
      return book.acquire_status === filter.value;
  }
}

/**
 * Parses a series position like `3`, `#3`, `3.5`, or `Book 3` for numeric sort.
 *
 * @param raw - Series index string from the store.
 * @returns Parsed number, or `null` when absent / non-numeric.
 */
export function parseSeriesIndex(raw: string | null | undefined): number | null {
  if (!raw?.trim()) return null;
  const m = raw.trim().match(/(\d+(?:\.\d+)?)/);
  if (!m) return null;
  const n = Number(m[1]);
  return Number.isFinite(n) ? n : null;
}

/**
 * Ascending series-number compare; missing indexes sort last.
 *
 * @param a - Left series index.
 * @param b - Right series index.
 * @returns Negative / zero / positive compare result.
 */
export function compareSeriesIndex(
  a: string | null | undefined,
  b: string | null | undefined,
): number {
  const na = parseSeriesIndex(a);
  const nb = parseSeriesIndex(b);
  if (na != null && nb != null && na !== nb) return na - nb;
  if (na != null && nb == null) return -1;
  if (na == null && nb != null) return 1;
  return (a ?? "").localeCompare(b ?? "", undefined, {
    sensitivity: "base",
    numeric: true,
  });
}

/**
 * Returns a new array of books sorted by the given key.
 *
 * @param books - Library rows.
 * @param sort - Sort key.
 * @returns Sorted copy (stable enough for UI tables).
 */
export function sortBooks(books: BookRecord[], sort: SortKey): BookRecord[] {
  const out = [...books];
  const cmp = (a: string, b: string) =>
    a.localeCompare(b, undefined, { sensitivity: "base" });
  out.sort((a, b) => {
    switch (sort) {
      case "author":
        return (
          cmp(a.authors ?? "", b.authors ?? "") || cmp(a.title, b.title)
        );
      case "series":
        return (
          cmp(a.series ?? "\uffff", b.series ?? "\uffff") ||
          compareSeriesIndex(a.series_index, b.series_index) ||
          cmp(a.title, b.title)
        );
      case "purchased": {
        // Unknown purchase dates sort last (not first) for descending order.
        const aKey = a.purchased_at?.trim() || "";
        const bKey = b.purchased_at?.trim() || "";
        if (!aKey && !bKey) return cmp(a.title, b.title);
        if (!aKey) return 1;
        if (!bKey) return -1;
        return bKey.localeCompare(aKey) || cmp(a.title, b.title);
      }
      case "added":
        return b.created_at.localeCompare(a.created_at);
      case "length":
        return (b.length_minutes ?? 0) - (a.length_minutes ?? 0);
      case "title":
      default:
        return cmp(a.title, b.title) || a.uuid.localeCompare(b.uuid);
    }
  });
  return out;
}

/**
 * Formats runtime minutes as `Xh Ym` (or `—` when unknown).
 *
 * @param minutes - Length in minutes, or `null`.
 * @returns Human-readable duration.
 */
export function formatDuration(minutes: number | null): string {
  if (minutes == null || minutes <= 0) return "—";
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  if (h === 0) return `${m}m`;
  if (m === 0) return `${h}h`;
  return `${h}h ${m}m`;
}

/**
 * Formats an ISO date for library tables.
 *
 * @param iso - ISO timestamp, or `null`.
 * @returns Locale short date, raw string on parse failure, or `—`.
 */
export function formatDate(iso: string | null): string {
  if (!iso) return "—";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
