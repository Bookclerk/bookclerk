// The authored JSON is the only metadata source for the published catalog,
// inert card pool and complete no-script fallback stack.
import records from "./src/catalog-records.json" with { type: "json" };
import { escapeHtml } from "./render.ts";

/** An English audiobook edition used in the decorative catalog. */
export interface CatalogRecord {
  /** Book title, without marketing subtitles or edition suffixes. */
  title: string;
  /** Stable illustrative catalog number; leading zeroes are significant. */
  number: string;
  /** Credited author or coauthors for this edition. */
  author: string;
  /** All credited narrators, in the source edition's display order. */
  narrators: readonly string[];
  /** Complete runtime of this edition in whole minutes, greater than zero. */
  durationMinutes: number;
  /** Narrative series and its position; zero denotes a novel-length prequel. */
  series?: {
    /** Short series name printed on the card. */
    name: string;
    /** Reading position within this series; Foundation uses publication order. */
    number: number;
  };
  /** Shared fictional universe, when broader than the narrative series. */
  universe?: string;
  /** Illustrative purchase source, not a claim about a visitor's library. */
  bought: string;
  /** Illustrative destination path, never read from the visitor's machine. */
  shelved: string;
  /** Public catalog page identifying the exact audiobook edition. */
  sourceUrl: string;
  /** Date its author, narrator credits and runtime were verified, as YYYY-MM-DD. */
  verifiedOn: string;
}

/** A narrative series with shared context and individually verified editions. */
export interface CatalogSeries {
  /** Display name inherited by each book's series row. */
  name: string;
  /** Shared fictional universe, inherited by the series' books. */
  universe?: string;
  /** Books in series order; shared series/universe fields are not repeated. */
  books: readonly (Omit<CatalogRecord, "series" | "universe"> & {
    /** Position in the series; zero denotes a novel-length prequel. */
    seriesNumber: number;
  })[];
}

/** The editable JSON layout: narrative series followed by standalone novels. */
export interface CatalogSource {
  /** Named series, each owning its ordered list of editions. */
  series: readonly CatalogSeries[];
  /** Independent novels, optionally belonging to a shared universe. */
  standalones: readonly CatalogRecord[];
}

/** One equally weighted choice in the per-load sample. */
export interface CatalogGroup {
  /** Stable namespaced identity for a universe, series or standalone book. */
  key: string;
  /** Human-readable group name for maintenance and inspection. */
  name: string;
  /** Editions with equal conditional probability once this group is selected. */
  books: readonly CatalogRecord[];
}

/** The grouped JSON, published unchanged as a Worker asset. */
export const catalogSource: CatalogSource = records;

/** All editions with shared series/universe context restored for rendering. */
export const catalogRecords: readonly CatalogRecord[] = [
  ...catalogSource.series.flatMap((series) =>
    series.books.map(({ seriesNumber, ...book }) => ({
      ...book,
      series: { name: series.name, number: seriesNumber },
      ...(series.universe ? { universe: series.universe } : {}),
    })),
  ),
  ...catalogSource.standalones,
];

/** Returns the namespaced sampling identity, keeping a shared universe together. */
function groupKey(record: CatalogRecord): string {
  return record.universe
    ? `universe:${record.universe}`
    : record.series
      ? `series:${record.series.name}`
      : `book:${record.number}`;
}

/** Builds equally weighted groups without changing their authored book order. */
function samplingGroups(): readonly CatalogGroup[] {
  const groups = new Map<string, { key: string; name: string; books: CatalogRecord[] }>();
  for (const record of catalogRecords) {
    const key = groupKey(record);
    const existing = groups.get(key);
    if (existing) existing.books.push(record);
    else
      groups.set(key, {
        key,
        name: record.universe ?? record.series?.name ?? record.title,
        books: [record],
      });
  }
  return [...groups.values()];
}

/** Equal-weight sampling groups; standalone novels each occupy their own group. */
export const catalogGroups: readonly CatalogGroup[] = samplingGroups();

/**
 * Renders one complete sheet with optional series context and compact credits.
 *
 * @param record - The verified edition and its illustrative library fields.
 * @returns Escaped markup; large narrator ensembles retain full credits in a tooltip.
 */
function catalogCard(record: CatalogRecord): string {
  const series = [
    record.series ? `${record.series.name} #${record.series.number}` : "",
    record.universe ?? "",
  ]
    .filter(Boolean)
    .join(" · ");
  const narrators = record.narrators.join(", ");
  const narrator =
    record.narrators.length > 2
      ? `${record.narrators[0]} + ${record.narrators.length - 1}`
      : narrators;
  const hours = Math.floor(record.durationMinutes / 60);
  const minutes = record.durationMinutes % 60;
  const length = `${hours ? `${hours} h` : ""}${hours && minutes ? " " : ""}${minutes ? `${minutes} min` : ""}`;
  const fields: readonly (readonly [string, string, string?])[] = [
    ["Author", record.author],
    ...(series ? [["Series", series] as const] : []),
    ["Narrator", narrator, narrators],
    ["Length", length],
    ["Bought", record.bought],
    ["Shelved", record.shelved],
  ];
  return `<div class="catalog-card" data-record="${escapeHtml(record.number)}" data-group="${escapeHtml(groupKey(record))}">
  <div class="card-title"><strong>${escapeHtml(record.title)}</strong><span class="faint">No. ${escapeHtml(record.number)}</span></div>
  <dl>${fields.map(([label, value, detail]) => `<dt>${label}</dt><dd${detail ? ` title="${escapeHtml(detail)}"` : ""}>${escapeHtml(value)}</dd>`).join("")}</dl>
  <span class="stamp">ACQUIRED</span><span class="stamp stamp-die">ACQUIRED</span>
</div>`;
}

/**
 * Renders the complete two-to-four-card no-script fallback stack.
 *
 * @param selection - Editions to show; defaults to one book from each of the first four groups.
 * @returns Escaped sheets, including each record's stamps.
 * @throws {Error} When the selection cannot form the supported stack.
 */
export function catalogCards(
  selection: readonly CatalogRecord[] = catalogGroups
    .slice(0, 4)
    .flatMap((group) => group.books.slice(0, 1)),
): string {
  if (selection.length < 2 || selection.length > 4)
    throw new Error("The catalog stack needs two to four records");
  return selection.map(catalogCard).join("\n");
}

/** Renders all editions into the inert pool used for per-load browser sampling. */
export function catalogPool(): string {
  return catalogGroups
    .map(
      (group) =>
        `<div data-catalog-group="${escapeHtml(group.key)}">${group.books.map(catalogCard).join("\n")}</div>`,
    )
    .join("\n");
}
