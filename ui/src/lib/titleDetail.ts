import type {
  AcquireStatus,
  WorkDetail,
  BranchAvailability,
  PurchaseHint,
  QueueWisher,
  StoreEdition,
  TitleReview,
} from "@/lib/api";
import { acquirableEditions } from "@/lib/api";
import { withAudibleEditionFromAsin, type CatalogTitle } from "@/lib/catalogTitle";

/**
 * Shared detail model for Discover, Wishlist, and Library dialogs.
 */
export type TitleDetail = {
  title: string;
  subtitle?: string | null;
  authors?: string | null;
  narrators?: string | null;
  series?: string | null;
  series_index?: string | null;
  asin?: string | null;
  isbn?: string | null;
  cover_url?: string | null;
  /** Same-origin path of the processed cover (see `coverSrc`). */
  cover_path?: string | null;
  /**
   * Held editions not yet acquired or on their way, for a library work.
   * When set, the dialog offers acquire while it is above zero, even if
   * another edition is already acquired; otherwise `acquire_status` decides.
   */
  acquirable_editions?: number | null;
  /** An acquired edition whose local cover is preferred when present. */
  library_edition_id?: string | null;
  /** The library work this detail shows, when it is one. */
  work_id?: string | null;
  description?: string | null;
  publisher?: string | null;
  length_minutes?: number | null;
  published_at?: string | null;
  categories?: string | null;
  tags?: string | null;
  language?: string | null;
  /** `true` abridged / `false` unabridged when known. */
  is_abridged?: boolean | null;
  rating_overall?: number | null;
  rating_performance?: number | null;
  rating_story?: number | null;
  /** Star-rating sample size (Audible). */
  rating_count?: number | null;
  /** Written review count (Audible). */
  review_count?: number | null;
  /** Sample of helpful Audible customer reviews. */
  reviews?: TitleReview[];
  work_key?: string | null;
  notes?: string | null;
  reasons?: string[];
  /** Branches of this library that currently list this title. */
  available_through?: BranchAvailability[];
  wishlist_uuid?: string | null;
  store_editions?: StoreEdition[];
  sources?: string[];
  /** Seeded hints; modal may refresh when showCommerce. */
  purchase_hints?: PurchaseHint[];
  /** People who have this title on an open wishlist. */
  wishers?: QueueWisher[];
  /** Distinct wisher count (may exceed `wishers.length`). */
  wish_count?: number;
  acquire_status?: AcquireStatus;
  source?: string | null;
  marketplace?: string | null;
  account_id?: string | null;
  product_id?: string | null;
  content_kind?: string | null;
  storage_key?: string | null;
  error_message?: string | null;
  created_at?: string | null;
  updated_at?: string | null;
  /** Show store links + live pricing (Discover / Wishlist). */
  showCommerce?: boolean;
};

/**
 * Builds a {@link TitleDetail} from a {@link CatalogTitle}.
 *
 * @param title - Unified catalog title.
 * @param extras - Optional field overrides.
 * @returns Detail model with commerce enabled by default.
 */
export function titleDetailFromCatalog(
  title: CatalogTitle,
  extras?: Partial<TitleDetail>,
): TitleDetail {
  return withAudibleEditionFromAsin({
    title: title.title,
    subtitle: title.subtitle,
    authors: title.authors,
    narrators: title.narrators,
    series: title.series,
    series_index: title.series_index,
    asin: title.asin,
    isbn: title.isbn,
    cover_url: title.cover_url,
    cover_path: title.cover_path,
    work_key: title.work_key,
    notes: title.notes,
    reasons: title.reasons,
    available_through: title.available_through,
    wishlist_uuid: title.wishlist_uuid,
    store_editions: title.store_editions,
    sources: title.sources,
    purchase_hints: title.purchase_hints,
    wishers: title.wishers,
    wish_count: title.wish_count,
    description: title.description,
    publisher: title.publisher,
    length_minutes: title.length_minutes,
    published_at: title.published_at,
    categories: title.genres,
    language: title.language,
    is_abridged: title.is_abridged,
    showCommerce: true,
    ...extras,
  });
}

/**
 * Builds a {@link TitleDetail} from a library work's detail.
 *
 * The work's own fields come first; edition fields (subtitle, runtime,
 * cover, store ids) come from its first edition that has them, audio
 * editions first.
 *
 * @param work - The work from `/api/library/works/{id}`.
 * @param status - The work's acquire state from its list row.
 * @returns Detail model with commerce disabled.
 */
export function titleDetailFromWork(work: WorkDetail, status: AcquireStatus): TitleDetail {
  const editions = [...work.editions].sort(
    (a, b) => Number(b.media_kind === "audio") - Number(a.media_kind === "audio"),
  );
  const first = <T>(pick: (e: (typeof editions)[number]) => T | null | undefined) => {
    for (const e of editions) {
      const v = pick(e);
      if (v !== null && v !== undefined && v !== "") return v;
    }
    return null;
  };
  const listings = editions.flatMap((e) => e.listings);
  const isAsin = (id: string) => /^B0[0-9A-Z]{8}$/i.test(id);
  const isIsbn = (id: string) => /^(97[89])?\d{9}[\dXx]$/.test(id.replace(/-/g, ""));
  const abridgement = first((e) => (e.abridgement === "unknown" ? null : e.abridgement));
  const listing = listings[0];
  return {
    title: work.title,
    subtitle: first((e) => e.subtitle),
    authors: work.authors.length > 0 ? work.authors.join(", ") : null,
    series: work.series,
    series_index: work.series_position,
    asin: listings.find((l) => isAsin(l.product_id))?.product_id ?? null,
    isbn: listings.find((l) => isIsbn(l.product_id))?.product_id ?? null,
    cover_path: first((e) => e.cover_path),
    library_edition_id: first((e) => (e.acquire_status === "acquired" ? e.id : null)),
    work_id: work.id,
    description: work.description,
    length_minutes: first((e) => e.duration_minutes),
    published_at: first((e) => e.published_at),
    language: first((e) => e.language),
    is_abridged: abridgement === null ? null : abridgement === "abridged",
    tags: work.state?.tags ?? null,
    rating_overall: work.state?.rating_overall ?? null,
    rating_performance: work.state?.rating_performance ?? null,
    rating_story: work.state?.rating_story ?? null,
    acquire_status: status,
    acquirable_editions: acquirableEditions(work).length,
    source: listing?.plugin_key ?? null,
    marketplace: listing?.marketplace ?? null,
    product_id: listing?.product_id ?? null,
    content_kind: work.kind,
    showCommerce: false,
  };
}
