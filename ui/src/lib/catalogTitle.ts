import type {
  CatalogSearchFilters,
  CatalogSearchHit,
  CatalogSearchSort,
  GlobalQueueEntry,
  PurchaseHint,
  Recommendation,
  StoreEdition,
  TitleMeta,
  TitleRequest,
} from "@/lib/api";
/**
 * Unified title shape for Discover / Wishlist detail dialogs and result rows.
 */
export type CatalogTitle = {
  work_key: string;
  title: string;
  authors: string | null;
  narrators: string | null;
  series: string | null;
  series_index?: string | null;
  asin: string | null;
  isbn: string | null;
  cover_url: string | null;
  store_editions: StoreEdition[];
  sources: string[];
  reasons?: string[];
  purchase_hints?: PurchaseHint[];
  subtitle?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  genres?: string | null;
  language?: string | null;
  /** `true` abridged / `false` unabridged when known. */
  is_abridged?: boolean | null;
  /** Audible community overall rating (0–5), when known. */
  rating_overall?: number | null;
  /** Present when the title is already on the current user’s wishlist. */
  wishlist_uuid?: string | null;
  notes?: string | null;
};

/**
 * Client-side catalog filter categories (legacy / local lists).
 */
export type CatalogFilterKind =
  | "all"
  | "authors"
  | "series"
  | "narrators"
  | "genres"
  | "sources";
/**
 * Alias of {@link CatalogSearchSort} for Discover UI state.
 */
export type CatalogSortKey = CatalogSearchSort;

/**
 * UI labels for {@link CatalogFilterKind}.
 */
export const CATALOG_FILTER_KINDS: { value: CatalogFilterKind; label: string }[] = [
  { value: "all", label: "All" },
  { value: "authors", label: "Authors" },
  { value: "series", label: "Series" },
  { value: "narrators", label: "Narrators" },
  { value: "genres", label: "Genres" },
  { value: "sources", label: "Sources" },
];

/**
 * Storefront search facet passed as `?field=` on `/api/discover/search`.
 */
export type CatalogSearchField = "author" | "narrator" | "series" | "genre";

/**
 * Narrator name excluded when “Hide Virtual Voice” is enabled.
 */
export const VIRTUAL_VOICE_EXCLUDE = "Virtual Voice";

/**
 * Composes a Discover catalog search + facet filter from a title-detail meta link.
 *
 * @param kind - Metadata link kind (authors / narrators / series / genres).
 * @param value - Linked label text.
 * @returns Search query, field, sort, and matching include filters.
 */
export function discoverSearchFromMeta(
  kind: "authors" | "narrators" | "series" | "genres",
  value: string,
): {
  q: string;
  field: CatalogSearchField;
  sort: CatalogSortKey;
  authors?: string[];
  narrators?: string[];
  series?: string[];
  genres?: string[];
  hideVirtualVoice: boolean;
} {
  const q = value.trim();
  const base = { q, sort: "relevance" as const, hideVirtualVoice: true };
  switch (kind) {
    case "series":
      return { ...base, field: "series", series: [q] };
    case "authors":
      return { ...base, field: "author", authors: [q] };
    case "narrators":
      return { ...base, field: "narrator", narrators: [q] };
    case "genres":
      return { ...base, field: "genre", genres: [q] };
  }
}

/**
 * UI labels for Discover catalog sort keys.
 */
export const CATALOG_SORT_OPTIONS: { value: CatalogSortKey; label: string }[] = [
  { value: "relevance", label: "Relevance" },
  { value: "popularity", label: "Popularity" },
  { value: "rating", label: "Rating" },
  { value: "title", label: "Title" },
  { value: "author", label: "Author" },
  { value: "price", label: "Price" },
  { value: "length", label: "Runtime" },
];

/**
 * Known storefront ids for prefs / empty facet fallbacks.
 */
export const CATALOG_SOURCE_IDS = [
  "audible",
  "chirp",
  "libro",
  "graphicaudio",
] as const;

/**
 * Discover runtime length buckets for server filters.
 */
export type RuntimeBucket = "any" | "under6" | "6to12" | "12to20" | "over20";

/**
 * UI options and minute bounds for {@link RuntimeBucket}.
 */
export const RUNTIME_BUCKET_OPTIONS: {
  value: RuntimeBucket;
  label: string;
  min?: number;
  max?: number;
}[] = [
  { value: "any", label: "Any length" },
  { value: "under6", label: "Under 6 hours", max: 359 },
  { value: "6to12", label: "6–12 hours", min: 360, max: 720 },
  { value: "12to20", label: "12–20 hours", min: 721, max: 1200 },
  { value: "over20", label: "20 hours & up", min: 1201 },
];

/**
 * Maps a runtime bucket to server min/max length filters.
 *
 * @param bucket - Selected runtime bucket.
 * @returns Optional `min_length_minutes` / `max_length_minutes`.
 */
export function runtimeBucketBounds(bucket: RuntimeBucket): {
  min_length_minutes?: number;
  max_length_minutes?: number;
} {
  const opt = RUNTIME_BUCKET_OPTIONS.find((o) => o.value === bucket);
  if (!opt || bucket === "any") return {};
  return {
    min_length_minutes: opt.min,
    max_length_minutes: opt.max,
  };
}

/**
 * Default sort direction for a catalog sort key.
 *
 * @param sort - Catalog sort key.
 * @returns `asc` for title/author/length; otherwise `desc`.
 */
export function defaultSortDirFor(sort: CatalogSortKey): "asc" | "desc" {
  if (sort === "title" || sort === "author" || sort === "length") return "asc";
  return "desc";
}

/**
 * Wire value for “no hard language filter” in the Discover language control.
 */
export const CATALOG_LANGUAGE_ALL = "__all__";

/**
 * Language select options for Discover (includes {@link CATALOG_LANGUAGE_ALL}).
 */
export const CATALOG_LANGUAGE_OPTIONS: { value: string; label: string }[] = [
  { value: "en", label: "English" },
  { value: "es", label: "Spanish" },
  { value: "fr", label: "French" },
  { value: "de", label: "German" },
  { value: "it", label: "Italian" },
  { value: "pt", label: "Portuguese" },
  { value: "ja", label: "Japanese" },
  { value: "zh", label: "Chinese" },
  { value: "ko", label: "Korean" },
  { value: "nl", label: "Dutch" },
  { value: "sv", label: "Swedish" },
  { value: "da", label: "Danish" },
  { value: "no", label: "Norwegian" },
  { value: "fi", label: "Finnish" },
  { value: "pl", label: "Polish" },
  { value: "ru", label: "Russian" },
  { value: "ar", label: "Arabic" },
  { value: "hi", label: "Hindi" },
  { value: "tr", label: "Turkish" },
  { value: CATALOG_LANGUAGE_ALL, label: "All languages" },
];

/**
 * Maps Discover language control → hard filter / all-languages flag.
 *
 * @param filterLanguage - Selected language code or {@link CATALOG_LANGUAGE_ALL}.
 * @returns Server search language options.
 */
export function catalogLanguageSearchOpts(filterLanguage: string): {
  languages?: string[];
  allLanguages?: boolean;
} {
  if (filterLanguage === CATALOG_LANGUAGE_ALL) {
    return { allLanguages: true };
  }
  const code = filterLanguage.trim().toLowerCase();
  if (!code) return { allLanguages: true };
  return { languages: [code] };
}

/**
 * Options for the language `<select>`, ensuring the browser default appears.
 *
 * @param preferred - Preferred BCP-47 primary tag (default: browser language).
 * @returns Value/label pairs for the control.
 */
export function catalogLanguageSelectOptions(
  preferred = preferredCatalogLanguage(),
): { value: string; label: string }[] {
  if (
    !preferred ||
    preferred === CATALOG_LANGUAGE_ALL ||
    CATALOG_LANGUAGE_OPTIONS.some((o) => o.value === preferred)
  ) {
    return CATALOG_LANGUAGE_OPTIONS;
  }
  const label = preferred.toUpperCase();
  const opts = [...CATALOG_LANGUAGE_OPTIONS];
  opts.splice(opts.length - 1, 0, { value: preferred, label });
  return opts;
}

/**
 * Builds the server filter payload from UI multi-select state.
 *
 * @param opts - Include lists, exclusions, rating, and runtime bucket.
 * @returns `CatalogSearchFilters` for `searchCatalog`.
 */
export function buildCatalogSearchFilters(opts: {
  authors: string[];
  narrators: string[];
  series: string[];
  genres: string[];
  /** @deprecated prefer excludeSources */
  sources?: string[];
  excludeSources?: string[];
  languages?: string[];
  hideVirtualVoice: boolean;
  minRating?: number | null;
  runtimeBucket?: RuntimeBucket;
}): CatalogSearchFilters {
  const runtime = runtimeBucketBounds(opts.runtimeBucket ?? "any");
  return {
    authors: opts.authors,
    narrators: opts.narrators,
    series: opts.series,
    genres: opts.genres,
    sources: opts.sources?.length ? opts.sources : undefined,
    exclude_sources: opts.excludeSources?.length
      ? opts.excludeSources
      : undefined,
    languages: opts.languages?.length ? opts.languages : undefined,
    exclude_narrators: opts.hideVirtualVoice ? [VIRTUAL_VOICE_EXCLUDE] : [],
    min_rating: opts.minRating && opts.minRating > 0 ? opts.minRating : undefined,
    ...runtime,
  };
}

/**
 * Human-readable label for a storefront plugin id in Discover filters.
 *
 * Falls back to the raw id when the store is unknown so future plugins still render.
 *
 * @param source - Store id (`audible`, `libro`, …).
 * @returns Display name for UI chrome.
 */
export function storeLabel(source: string): string {
  switch (source.toLowerCase()) {
    case "audible":
      return "Audible";
    case "libro":
      return "Libro.fm";
    case "chirp":
      return "Chirp";
    case "graphicaudio":
      return "GraphicAudio";
    default:
      return source;
  }
}

/** First-party storefront / integration domains for Google favicon URLs. */
const STORE_FAVICON_DOMAINS: Record<string, string> = {
  audible: "audible.com",
  chirp: "chirpbooks.com",
  libro: "libro.fm",
  "libro.fm": "libro.fm",
  graphicaudio: "graphicaudio.com",
  audiobookshelf: "audiobookshelf.org",
};

/**
 * Builds a Google S2 favicon URL for a domain.
 *
 * @param domain - Hostname such as `audible.com`.
 * @returns Favicon image URL.
 */
export function googleFaviconUrl(domain: string): string {
  return `https://www.google.com/s2/favicons?domain=${encodeURIComponent(domain)}&sz=128`;
}

/**
 * Favicon URL for a storefront id (`audible`, `libro`, …), if known.
 *
 * @param source - Store id.
 * @returns Favicon URL, or `undefined` when the domain is unknown.
 */
export function storeFaviconUrl(source: string): string | undefined {
  const domain = STORE_FAVICON_DOMAINS[source.trim().toLowerCase()];
  return domain ? googleFaviconUrl(domain) : undefined;
}

/**
 * Primary BCP-47 language tag from the browser (default `en`).
 *
 * @returns Two/three-letter language code.
 */
export function preferredCatalogLanguage(): string {
  try {
    const raw =
      (typeof navigator !== "undefined" &&
        (navigator.languages?.[0] || navigator.language)) ||
      "en";
    const primary = String(raw).trim().toLowerCase().split(/[-_]/)[0];
    if (primary && /^[a-z]{2,3}$/.test(primary)) return primary;
  } catch {
    /* ignore */
  }
  return "en";
}

const LANGUAGE_NAME_TO_CODE: Record<string, string> = {
  english: "en",
  eng: "en",
  spanish: "es",
  español: "es",
  espanol: "es",
  spa: "es",
  french: "fr",
  français: "fr",
  francais: "fr",
  fra: "fr",
  german: "de",
  deutsch: "de",
  deu: "de",
  ger: "de",
  italian: "it",
  italiano: "it",
  ita: "it",
  portuguese: "pt",
  português: "pt",
  portugues: "pt",
  por: "pt",
  japanese: "ja",
  jpn: "ja",
  chinese: "zh",
  chi: "zh",
  zho: "zh",
  korean: "ko",
  kor: "ko",
};

function normalizeCatalogLanguage(raw: string | null | undefined): string | null {
  const s = (raw ?? "").trim().toLowerCase();
  if (!s) return null;
  const primary = s.split(/[-_]/)[0]?.trim() ?? "";
  if (!primary) return null;
  if (LANGUAGE_NAME_TO_CODE[primary]) return LANGUAGE_NAME_TO_CODE[primary];
  if (/^[a-z]{2,3}$/.test(primary)) return primary;
  return null;
}

/**
 * Soft language rank for ordering search hits.
 *
 * @param hitLanguage - Language on the hit.
 * @param preferred - Preferred language (default: browser).
 * @returns `0` preferred, `1` unknown, `2` other.
 */
export function catalogLanguageRank(
  hitLanguage: string | null | undefined,
  preferred = preferredCatalogLanguage(),
): number {
  const hit = normalizeCatalogLanguage(hitLanguage);
  if (!hit) return 1;
  return hit === preferred ? 0 : 2;
}

/**
 * Stable soft-sort: preferred language first, then unknown, then other.
 *
 * @param items - Rows with optional `language`.
 * @param preferred - Preferred language code.
 * @returns New array sorted by {@link catalogLanguageRank}.
 */
export function preferCatalogLanguageOrder<T extends { language?: string | null }>(
  items: T[],
  preferred = preferredCatalogLanguage(),
): T[] {
  return [...items].sort(
    (a, b) =>
      catalogLanguageRank(a.language, preferred) -
      catalogLanguageRank(b.language, preferred),
  );
}

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
 * Formats series name + book number for list/card rows (`Sun Eater #3`).
 *
 * @param series - Series name.
 * @param seriesIndex - Optional book number.
 * @returns Label, or `null` when series is empty.
 */
export function formatSeriesLabel(
  series: string | null | undefined,
  seriesIndex?: string | null,
): string | null {
  const name = series?.trim();
  if (!name) return null;
  const index = seriesIndex?.trim();
  return index ? `${name} #${index}` : name;
}

/**
 * Splits genre / category blobs (`Fantasy; Epic` / comma / pipe).
 *
 * @param raw - Combined list string.
 * @returns Trimmed non-empty parts.
 */
export function splitMetaList(raw: string | null | undefined): string[] {
  if (!raw?.trim()) return [];
  return raw
    .split(/[,;|]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/**
 * Compact purchase price line; prefers dual member/list when both exist.
 *
 * @param hint - Price label fields from a purchase hint.
 * @returns Display string, or `null` when no prices.
 */
export function formatPurchasePrices(hint: {
  price_label?: string | null;
  member_price_label?: string | null;
  list_price_label?: string | null;
}): string | null {
  const member = hint.member_price_label?.trim() || null;
  const list = hint.list_price_label?.trim() || null;
  if (member && list && member !== list) {
    return `${member} member · ${list}`;
  }
  return (
    hint.price_label?.trim() ||
    member ||
    list ||
    null
  );
}

/**
 * Single best price for shelf cards (member / primary only — no dual list).
 *
 * @param hint - Price label fields from a purchase hint.
 * @returns Display string, or `null` when no prices.
 */
export function formatShelfBestPrice(hint: {
  price_label?: string | null;
  member_price_label?: string | null;
  list_price_label?: string | null;
}): string | null {
  return (
    hint.member_price_label?.trim() ||
    hint.price_label?.trim() ||
    hint.list_price_label?.trim() ||
    null
  );
}

/**
 * Maps a catalog search hit into a {@link CatalogTitle}.
 *
 * @param hit - Search hit.
 * @param wishlistUuid - Optional wishlist uuid when already wished.
 * @returns Unified catalog title.
 */
export function catalogTitleFromHit(
  hit: CatalogSearchHit,
  wishlistUuid?: string | null,
): CatalogTitle {
  return {
    work_key: hit.work_key,
    title: hit.title,
    authors: hit.authors,
    narrators: hit.narrators,
    series: hit.series,
    series_index: hit.series_index ?? null,
    asin: hit.asin,
    isbn: hit.isbn,
    cover_url: hit.cover_url ?? null,
    store_editions: hit.store_editions ?? [],
    sources: hit.sources ?? [],
    purchase_hints: hit.purchase_hints,
    subtitle: hit.subtitle,
    description: hit.description,
    publisher: hit.publisher,
    length_minutes: hit.length_minutes,
    published_at: hit.published_at,
    genres: hit.genres,
    language: hit.language,
    is_abridged: hit.is_abridged ?? null,
    rating_overall: hit.rating_overall ?? null,
    wishlist_uuid: wishlistUuid ?? null,
  };
}

/**
 * Maps a Discover recommendation into a {@link CatalogTitle}.
 *
 * @param rec - Recommendation row.
 * @param wishlistUuid - Optional wishlist uuid override.
 * @returns Unified catalog title.
 */
export function catalogTitleFromRec(
  rec: Recommendation,
  wishlistUuid?: string | null,
): CatalogTitle {
  const editions = rec.store_editions ?? [];
  const sources =
    editions.length > 0
      ? [...new Set(editions.map((e) => e.source))]
      : rec.candidate_source
        ? [rec.candidate_source]
        : [];
  const work_key =
    rec.work_key?.trim() ||
    (rec.asin ? `asin:${rec.asin.toUpperCase()}` : "") ||
    (rec.isbn ? `isbn:${rec.isbn.replace(/[^0-9Xx]/g, "").toUpperCase()}` : "") ||
    `soft:${rec.title.trim().toLowerCase()}`;
  return {
    work_key,
    title: rec.title,
    authors: rec.authors,
    narrators: rec.narrators,
    series: rec.series,
    series_index: rec.series_index,
    asin: rec.asin,
    isbn: rec.isbn,
    cover_url: rec.cover_url ?? null,
    store_editions: editions,
    sources,
    reasons: rec.reasons,
    purchase_hints: rec.purchase_hints,
    subtitle: rec.subtitle,
    description: rec.description,
    publisher: rec.publisher,
    length_minutes: rec.length_minutes,
    published_at: rec.published_at,
    genres: rec.genres,
    language: rec.language,
    wishlist_uuid: wishlistUuid ?? rec.request_uuid,
  };
}

/**
 * Maps a wishlist title-request into a {@link CatalogTitle}.
 *
 * @param req - Wishlist row.
 * @returns Unified catalog title (with `wishlist_uuid` set).
 */
export function catalogTitleFromRequest(req: TitleRequest): CatalogTitle {
  const editions = req.store_editions ?? [];
  const sources =
    editions.length > 0
      ? [...new Set(editions.map((e) => e.source))]
      : [];
  return {
    work_key: req.work_key || `soft:${req.title.trim().toLowerCase()}`,
    title: req.title,
    authors: req.authors,
    narrators: req.narrators ?? null,
    series: req.series ?? null,
    series_index: req.series_index ?? null,
    asin: req.asin,
    isbn: req.isbn,
    cover_url: req.cover_url ?? null,
    store_editions: editions,
    sources,
    purchase_hints: req.purchase_hints,
    description: req.description ?? null,
    subtitle: req.subtitle ?? null,
    publisher: req.publisher ?? null,
    length_minutes: req.length_minutes ?? null,
    published_at: req.published_at ?? null,
    genres: req.genres ?? null,
    language: req.language ?? null,
    wishlist_uuid: req.uuid,
    notes: req.notes,
  };
}

function pickStr(
  current: string | null | undefined,
  incoming: string | null | undefined,
): string | null {
  const cur = current?.trim();
  if (cur) return current ?? null;
  const next = incoming?.trim();
  return next ? incoming! : current ?? null;
}

function hasHtmlMarkup(raw: string): boolean {
  return /<[a-zA-Z]/.test(raw);
}

/**
 * Prefers richer store/Audnexus blurbs over short plain catalog teasers.
 *
 * HTML descriptions win over plain text; otherwise the longer string wins.
 *
 * @param a - Current description.
 * @param b - Incoming description.
 * @returns Preferred non-empty description, or `null`.
 */
export function pickBetterDescription(
  a: string | null | undefined,
  b: string | null | undefined,
): string | null {
  const left = a?.trim() ?? "";
  const right = b?.trim() ?? "";
  if (!left) return right || null;
  if (!right) return left || null;
  const leftHtml = hasHtmlMarkup(left);
  const rightHtml = hasHtmlMarkup(right);
  if (rightHtml && !leftHtml) return right;
  if (leftHtml && !rightHtml) return left;
  return right.length > left.length ? right : left;
}

/**
 * Returns whether a blurb looks like a full store description, not a truncated teaser.
 *
 * @param raw - Optional HTML/plain description.
 * @returns True when length and ending look complete.
 */
export function descriptionLooksComplete(raw: string | null | undefined): boolean {
  const t = raw?.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim() ?? "";
  if (t.length < 480) return false;
  // Catalog / merchandising blurbs often end mid-sentence with an ellipsis.
  if (/(\.\.\.|…)\s*$/.test(t)) return false;
  return true;
}

/**
 * Overlays public Audnexus / catalog metadata onto a sparse {@link CatalogTitle}.
 *
 * @param title - Existing title.
 * @param meta - Fetched metadata (no-op when nullish).
 * @returns Merged title.
 */
export function applyTitleMeta(title: CatalogTitle, meta: TitleMeta | null | undefined): CatalogTitle {
  if (!meta) return title;
  return {
    ...title,
    subtitle: pickStr(title.subtitle, meta.subtitle),
    authors: pickStr(title.authors, meta.authors),
    narrators: pickStr(title.narrators, meta.narrators),
    series: pickStr(title.series, meta.series),
    series_index: pickStr(title.series_index, meta.series_index),
    asin: pickStr(title.asin, meta.asin),
    isbn: pickStr(title.isbn, meta.isbn),
    cover_url: pickStr(title.cover_url, meta.cover_url),
    description: pickBetterDescription(title.description, meta.description),
    publisher: pickStr(title.publisher, meta.publisher),
    length_minutes: title.length_minutes ?? meta.length_minutes ?? null,
    published_at: pickStr(title.published_at, meta.published_at),
    genres: pickStr(title.genres, meta.categories),
    is_abridged: title.is_abridged ?? meta.is_abridged ?? null,
    language: pickStr(title.language, meta.language),
    rating_overall: title.rating_overall ?? meta.rating_overall ?? null,
  };
}

/**
 * Returns whether a title still needs public metadata enrichment.
 *
 * @param title - Catalog title.
 * @returns True when core fields (description, length, publisher, narrators, series index) are incomplete.
 */
export function titleNeedsMeta(title: CatalogTitle): boolean {
  const hasCore = Boolean(
    descriptionLooksComplete(title.description) &&
      title.length_minutes != null &&
      title.publisher?.trim() &&
      title.narrators?.trim(),
  );
  // Series without a book number still needs enrichment for ordered series views.
  // Genres are nice-to-have and must not force enrichment of every search hit
  // (that stacked Audnexus calls and could empty the results page on failure).
  const seriesComplete =
    !title.series?.trim() || Boolean(title.series_index?.trim());
  return !(hasCore && seriesComplete);
}

/**
 * Returns whether a shelf card should pull title-meta (includes missing overall rating).
 *
 * @param title - Catalog title.
 * @returns True when {@link titleNeedsMeta} or `rating_overall` is unknown.
 */
export function titleNeedsShelfMeta(title: CatalogTitle): boolean {
  return titleNeedsMeta(title) || title.rating_overall == null;
}

/**
 * Maps a global request-queue entry into a {@link CatalogTitle}.
 *
 * @param entry - Queue entry.
 * @param wishlistUuid - Optional wishlist uuid when the current user also wished it.
 * @returns Unified catalog title.
 */
export function catalogTitleFromQueueEntry(
  entry: GlobalQueueEntry,
  wishlistUuid?: string | null,
): CatalogTitle {
  const editions = entry.store_editions ?? [];
  const sources =
    editions.length > 0
      ? [...new Set(editions.map((e) => e.source))]
      : [];
  return {
    work_key: entry.work_key,
    title: entry.title,
    authors: entry.authors,
    narrators: entry.narrators ?? null,
    series: entry.series ?? null,
    series_index: entry.series_index ?? null,
    asin: entry.asin,
    isbn: entry.isbn,
    cover_url: entry.cover_url ?? null,
    store_editions: editions,
    sources,
    purchase_hints: entry.purchase_hints,
    description: entry.description ?? null,
    subtitle: entry.subtitle ?? null,
    publisher: entry.publisher ?? null,
    length_minutes: entry.length_minutes ?? null,
    published_at: entry.published_at ?? null,
    genres: entry.genres ?? null,
    language: entry.language ?? null,
    reasons: entry.reasons,
    wishlist_uuid: wishlistUuid ?? null,
  };
}

/**
 * Builds distinct local filter options from a title list.
 *
 * @param titles - Catalog titles.
 * @param kind - Filter category.
 * @returns Value/label pairs.
 */
export function catalogFilterValues(
  titles: CatalogTitle[],
  kind: CatalogFilterKind,
): { value: string; label: string }[] {
  switch (kind) {
    case "all":
      return [];
    case "authors":
      return uniqueSorted(titles.flatMap((t) => splitList(t.authors))).map((v) => ({
        value: v,
        label: v,
      }));
    case "series": {
      const named = uniqueSorted(
        titles.map((t) => t.series?.trim()).filter(Boolean) as string[],
      ).map((v) => ({ value: v, label: v }));
      const hasNone = titles.some((t) => !t.series?.trim());
      return hasNone ? [{ value: "__none__", label: "No series" }, ...named] : named;
    }
    case "narrators":
      return uniqueSorted(titles.flatMap((t) => splitList(t.narrators))).map((v) => ({
        value: v,
        label: v,
      }));
    case "genres":
      return uniqueSorted(titles.flatMap((t) => splitList(t.genres))).map((v) => ({
        value: v,
        label: v,
      }));
    case "sources":
      return uniqueSorted(titles.flatMap((t) => t.sources)).map((v) => ({
        value: v,
        label: storeLabel(v),
      }));
  }
}

/**
 * Returns whether a title matches a local catalog filter.
 *
 * @param title - Catalog title.
 * @param kind - Filter category.
 * @param value - Selected value (`all` / empty always matches).
 * @returns True when visible under the filter.
 */
export function catalogMatchesFilter(
  title: CatalogTitle,
  kind: CatalogFilterKind,
  value: string,
): boolean {
  if (kind === "all" || !value) return true;
  switch (kind) {
    case "authors":
      return splitList(title.authors).some(
        (a) => a.localeCompare(value, undefined, { sensitivity: "base" }) === 0,
      );
    case "series":
      if (value === "__none__") return !title.series?.trim();
      return (title.series ?? "").localeCompare(value, undefined, {
        sensitivity: "base",
      }) === 0;
    case "narrators":
      return splitList(title.narrators).some(
        (a) => a.localeCompare(value, undefined, { sensitivity: "base" }) === 0,
      );
    case "genres":
      return splitList(title.genres).some(
        (g) => g.localeCompare(value, undefined, { sensitivity: "base" }) === 0,
      );
    case "sources":
      return title.sources.some((s) => s.toLowerCase() === value.toLowerCase());
  }
}

/**
 * Client-sorts catalog titles for keys the server does not re-rank locally.
 *
 * Relevance / popularity / rating keep input order (server ranking).
 *
 * @param titles - Catalog titles.
 * @param sortKey - Sort key.
 * @returns Sorted copy or original order for server-ranked keys.
 */
export function sortCatalogTitles(
  titles: CatalogTitle[],
  sortKey: CatalogSortKey,
): CatalogTitle[] {
  // Server already ranks relevance/popularity/rating; keep stable order.
  if (
    sortKey === "relevance" ||
    sortKey === "popularity" ||
    sortKey === "rating"
  ) {
    return titles;
  }
  const copy = [...titles];
  const cmp = (a: string, b: string) =>
    a.localeCompare(b, undefined, { sensitivity: "base" });
  copy.sort((a, b) => {
    switch (sortKey) {
      case "author":
        return (
          cmp(a.authors ?? "", b.authors ?? "") || cmp(a.title, b.title)
        );
      case "title":
      default:
        return cmp(a.title, b.title);
    }
  });
  return copy;
}
