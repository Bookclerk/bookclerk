import type { AcquireStatus, MediaKind, WorkSort, WorkSummary } from "@/lib/api";
import { windowEntryIsYours, type WindowEditionView, type WindowEntryView } from "@/lib/join";

/**
 * Which books the Library list shows by where they are held: every
 * library (`all`), only this server's own (`this`), only books another
 * library holds (`other`), only the person's own books in other libraries
 * (`yours`), or one other library by host (`host:<host>`).
 */
export type LibraryPlace = "all" | "this" | "other" | "yours" | `host:${string}`;

/** One row of the merged Library list. */
export type LibraryListItem =
  | {
      /** A work of this server's own library. */
      kind: "work";
      /** The work as the works page lists it. */
      work: WorkSummary;
      /** The other libraries' entries placed on this work, if any. */
      elsewhere: WindowEntryView[];
    }
  | {
      /** A book only other libraries hold. */
      kind: "remote";
      /** Its merged entry from the libraries that hold it. */
      entry: WindowEntryView;
    };

/** The Library page's search, filters and order the merge follows. */
export interface LibraryMergeOptions {
  /** The page order (ignored while `q` is set: a search orders by relevance). */
  sort: WorkSort;
  /** The trimmed search text; empty for none. */
  q: string;
  /** The acquire-status filter. */
  status: AcquireStatus | "all";
  /** The format filter. */
  media: MediaKind | "all";
  /** True while a facet filter (genre, tag, ...) is applied. */
  facetApplied: boolean;
  /** Where the shown books are held. */
  place: LibraryPlace;
  /** True once the last page of this library's works is loaded. */
  complete: boolean;
}

/** Leading articles a title's sort key drops (as the works index does). */
const ARTICLES = ["the ", "a ", "an "];

/**
 * The sort key of a title, as the works index computes it: lower case,
 * with one leading English article dropped, so "The Hobbit" sorts under h.
 *
 * @param title - The display title.
 * @returns The key.
 */
export function sortTitle(title: string): string {
  const lower = title.trim().toLowerCase();
  for (const article of ARTICLES) {
    if (lower.startsWith(article)) {
      const rest = lower.slice(article.length).trimStart();
      if (rest) return rest;
    }
  }
  return lower;
}

/**
 * The sort key of an author list: its first name, last word first and
 * lower case ("Frank Herbert, Brian Herbert" sorts as "herbert, frank").
 *
 * @param authors - Comma-separated display names, or nothing.
 * @returns The key; empty when there is no author.
 */
export function sortAuthor(authors: string | null | undefined): string {
  const first = (authors ?? "").split(",")[0]?.trim() ?? "";
  const at = first.lastIndexOf(" ");
  const name = at > 0 ? `${first.slice(at + 1)}, ${first.slice(0, at)}` : first;
  return name.toLowerCase();
}

/**
 * The series sort key, as the works index computes it: `0`, the lower-case
 * series name, a separator and the position (its first number, the whole
 * part zero-padded to ten digits, or `~` when it has none); `1` for a book
 * in no series, so those sort last.
 *
 * @param series - The series name, or nothing.
 * @param position - The position in the series, or nothing.
 * @returns The key.
 */
export function sortSeries(
  series: string | null | undefined,
  position: string | null | undefined,
): string {
  if (!series) return "1";
  const found = /(\d+)(?:\.(\d+))?/.exec(position ?? "");
  let key = "~";
  if (found) {
    const whole = (found[1] ?? "").replace(/^0+/, "") || "0";
    key = whole.padStart(10, "0") + (found[2] ? `.${found[2]}` : "");
  }
  return `0${series.trim().toLowerCase()}\u0001${key}`;
}

/** The fields a search term can name, mapped to the editions' fields. */
const SEARCH_FIELDS: Record<string, (edition: WindowEditionView) => (string | null | undefined)[]> =
  {
    title: (e) => [e.title, e.subtitle],
    author: (e) => [e.authors],
    narrator: (e) => [e.narrators],
    series: (e) => [e.series],
    asin: (e) => [e.product_id],
    isbn: (e) => [e.product_id],
  };

/** One term of a search: an optional field and its lower-case text. */
interface SearchTerm {
  /** The field the term names (`author`, ...), or `null` for any field. */
  field: string | null;
  /** The text to find, lower case. */
  text: string;
}

/**
 * Splits a Library search into terms: `field:word`, `field:"a phrase"`,
 * `"a phrase"` or a bare word. A field the search does not know is read
 * as plain text.
 *
 * @param q - The search text.
 * @returns The terms, every one of which a book must match.
 */
export function searchTerms(q: string): SearchTerm[] {
  const terms: SearchTerm[] = [];
  const pattern = /(?:(\w+):)?(?:"([^"]*)"|(\S+))/g;
  for (const match of q.matchAll(pattern)) {
    const [whole, rawField, phrase, word] = match;
    const field = rawField?.toLowerCase() ?? null;
    if (field !== null && !(field in SEARCH_FIELDS)) {
      const text = whole.toLowerCase().replaceAll('"', "").trim();
      if (text) terms.push({ field: null, text });
      continue;
    }
    const text = (phrase ?? word ?? "").trim().toLowerCase();
    if (text) terms.push({ field, text });
  }
  return terms;
}

/**
 * True when an entry matches every search term in at least one edition.
 *
 * @param entry - A book other libraries hold.
 * @param terms - The search terms (see {@link searchTerms}).
 * @returns Whether it matches; any entry matches no terms.
 */
function matchesSearch(entry: WindowEntryView, terms: SearchTerm[]): boolean {
  return terms.every((term) =>
    entry.editions.some((edition) => {
      const fields = term.field
        ? (SEARCH_FIELDS[term.field]?.(edition) ?? [])
        : [edition.title, edition.subtitle, edition.authors, edition.narrators, edition.series];
      return fields.some(
        (value) => typeof value === "string" && value.toLowerCase().includes(term.text),
      );
    }),
  );
}

/**
 * The hosts of the libraries holding an entry, each once.
 *
 * @param entry - A book other libraries hold.
 * @returns For example `["books.example.com"]`.
 */
function entryHosts(entry: WindowEntryView): string[] {
  const hosts = new Set<string>();
  for (const edition of entry.editions) {
    let host = edition.owner_origin;
    try {
      host = new URL(edition.owner_origin).host || edition.owner_origin;
    } catch {
      // Not a URL: keep it as it is.
    }
    hosts.add(host);
  }
  return [...hosts];
}

/**
 * True when an entry is held where `place` asks.
 *
 * @param entry - A book other libraries hold.
 * @param place - The place filter.
 * @returns Whether it belongs in the list.
 */
function entryInPlace(entry: WindowEntryView, place: LibraryPlace): boolean {
  if (place === "all" || place === "other") return true;
  if (place === "this") return false;
  if (place === "yours") return windowEntryIsYours(entry);
  return entryHosts(entry).includes(place.slice("host:".length));
}

/**
 * True when a book only other libraries hold passes the filters. Such a
 * book has no acquire state or facet values in this library, so a status
 * or facet filter leaves it out; the format filter reads the formats the
 * libraries have (an ebook is a PDF there).
 *
 * @param entry - A book other libraries hold.
 * @param options - The page's filters.
 * @param terms - The search terms.
 * @returns Whether it shows.
 */
function remoteShows(
  entry: WindowEntryView,
  options: LibraryMergeOptions,
  terms: SearchTerm[],
): boolean {
  if (options.status !== "all" || options.facetApplied) return false;
  if (options.media !== "all") {
    const wanted = options.media === "audio" ? "audio" : options.media === "ebook" ? "pdf" : null;
    if (!wanted || !entry.editions.some((e) => e.formats.includes(wanted))) return false;
  }
  return entryInPlace(entry, options.place) && matchesSearch(entry, terms);
}

/** The fields a row sorts by, from a work or an entry's first edition. */
interface SortFields {
  /** Display title. */
  title: string;
  /** Comma-separated authors. */
  authors: string | null | undefined;
  /** Series name. */
  series: string | null | undefined;
  /** Position in the series. */
  position: string | null | undefined;
  /** Runtime in minutes; 0 or less is unknown. */
  length: number | null | undefined;
  /** When it was added (ISO-8601), if known. */
  added: string | null | undefined;
  /** When it was bought (ISO-8601), if known. */
  purchased: string | null | undefined;
}

/**
 * Compares two optional values newest or largest first, unknown last.
 *
 * @param a - The first value, or `null` when unknown.
 * @param b - The second value, or `null` when unknown.
 * @returns Negative when `a` comes first.
 */
function descending<T extends string | number>(a: T | null, b: T | null): number {
  if (a === null || b === null) return a === null ? (b === null ? 0 : 1) : -1;
  return a > b ? -1 : a < b ? 1 : 0;
}

/**
 * Compares two strings ascending.
 *
 * @param a - The first string.
 * @param b - The second string.
 * @returns Negative when `a` comes first.
 */
function ascending(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Compares two rows in a page order, as the works index orders them.
 *
 * @param sort - The order.
 * @param a - The first row's fields.
 * @param b - The second row's fields.
 * @returns Negative when `a` comes first, 0 when they tie.
 */
export function compareRows(sort: WorkSort, a: SortFields, b: SortFields): number {
  const byTitle = () => ascending(sortTitle(a.title), sortTitle(b.title));
  switch (sort) {
    case "title":
      return byTitle();
    case "author":
      return ascending(sortAuthor(a.authors), sortAuthor(b.authors)) || byTitle();
    case "series":
      return (
        ascending(sortSeries(a.series, a.position), sortSeries(b.series, b.position)) || byTitle()
      );
    case "length": {
      const known = (n: number | null | undefined) => (n && n > 0 ? n : null);
      return descending(known(a.length), known(b.length));
    }
    case "added":
      return descending(a.added || null, b.added || null);
    case "purchased":
      return descending(a.purchased || null, b.purchased || null);
  }
}

/**
 * The sort fields of a work of this library.
 *
 * @param work - The work.
 * @returns Its fields.
 */
function workFields(work: WorkSummary): SortFields {
  return {
    title: work.title,
    authors: work.authors,
    series: work.series,
    position: work.series_position,
    length: work.length_minutes,
    added: work.added_at,
    purchased: work.purchased_at,
  };
}

/**
 * The sort fields of a book other libraries hold: its first edition's,
 * with the longest runtime any edition has. It has no added or purchase
 * time in this library, so those orders put it last.
 *
 * @param entry - The entry.
 * @returns Its fields.
 */
function entryFields(entry: WindowEntryView): SortFields {
  const first = entry.editions[0];
  const lengths = entry.editions.map((e) => e.length_minutes ?? 0);
  return {
    title: first?.title ?? "",
    authors: first?.authors,
    series: first?.series,
    position: first?.series_index,
    length: lengths.length ? Math.max(...lengths) : null,
    added: null,
    purchased: null,
  };
}

/**
 * Merges this library's works with the books other libraries hold into
 * the one list the Library page shows.
 *
 * - A book another library holds that was placed on one of the works shown
 *   joins that work's row (`elsewhere`) instead of a row of its own.
 * - Every other book from another library gets a row, filtered by the
 *   page's search, filters and `place`, and placed among the works in the
 *   page order. While more pages of works are to come, only those that
 *   sort before the last work loaded show, so a row never jumps when the
 *   next page arrives. Under a search (relevance order) they follow this
 *   library's matches once those are all loaded.
 * - With `place` other than `all`, a work shows only when it is held where
 *   `place` asks (`this` keeps every work).
 *
 * @param works - This library's works, in page order.
 * @param entries - Every book other libraries hold (`fetchWindowBooks`).
 * @param options - The page's search, filters and order.
 * @returns The rows to show, in order.
 */
export function mergeLibraryList(
  works: WorkSummary[],
  entries: WindowEntryView[],
  options: LibraryMergeOptions,
): LibraryListItem[] {
  const byWork = new Map<string, WindowEntryView[]>();
  for (const entry of entries) {
    if (!entry.work_id) continue;
    const list = byWork.get(entry.work_id) ?? [];
    list.push(entry);
    byWork.set(entry.work_id, list);
  }
  const placed = new Set<string>();
  const local: LibraryListItem[] = [];
  for (const work of works) {
    const elsewhere = byWork.get(work.id) ?? [];
    for (const entry of elsewhere) placed.add(entry.key);
    const shown =
      options.place === "all" ||
      options.place === "this" ||
      elsewhere.some((entry) => entryInPlace(entry, options.place));
    if (shown) local.push({ kind: "work", work, elsewhere });
  }
  const terms = searchTerms(options.q);
  const remote = entries
    .filter((entry) => !placed.has(entry.key) && remoteShows(entry, options, terms))
    .map((entry) => ({ entry, fields: entryFields(entry) }));
  const sort: WorkSort = options.q ? "title" : options.sort;
  remote.sort((a, b) => compareRows(sort, a.fields, b.fields));

  if (options.q) {
    // Relevance order: another library's matches cannot be ranked against
    // this library's, so they follow them.
    return options.complete
      ? [...local, ...remote.map(({ entry }) => ({ kind: "remote" as const, entry }))]
      : local;
  }
  const merged: LibraryListItem[] = [];
  let next = 0;
  for (const item of local) {
    if (item.kind === "work") {
      const fields = workFields(item.work);
      while (next < remote.length && compareRows(sort, remote[next]!.fields, fields) < 0) {
        merged.push({ kind: "remote", entry: remote[next]!.entry });
        next += 1;
      }
    }
    merged.push(item);
  }
  // Past the last work loaded only once no more works are to come.
  const lastWork = works.at(-1);
  for (; next < remote.length; next += 1) {
    const { entry, fields } = remote[next]!;
    if (!options.complete && (!lastWork || compareRows(sort, fields, workFields(lastWork)) >= 0)) {
      break;
    }
    merged.push({ kind: "remote", entry });
  }
  return merged;
}
