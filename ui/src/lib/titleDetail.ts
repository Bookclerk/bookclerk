import type {
  AcquireStatus,
  BookRecord,
  PurchaseHint,
  StoreEdition,
  TitleReview,
} from "@/lib/api";
import type { CatalogTitle } from "@/lib/catalogTitle";

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
  /** Prefer local library cover when present. */
  library_uuid?: string | null;
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
  wishlist_uuid?: string | null;
  store_editions?: StoreEdition[];
  sources?: string[];
  /** Seeded hints; modal may refresh when showCommerce. */
  purchase_hints?: PurchaseHint[];
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
  return {
    title: title.title,
    subtitle: title.subtitle,
    authors: title.authors,
    narrators: title.narrators,
    series: title.series,
    series_index: title.series_index,
    asin: title.asin,
    isbn: title.isbn,
    cover_url: title.cover_url,
    work_key: title.work_key,
    notes: title.notes,
    reasons: title.reasons,
    wishlist_uuid: title.wishlist_uuid,
    store_editions: title.store_editions,
    sources: title.sources,
    purchase_hints: title.purchase_hints,
    description: title.description,
    publisher: title.publisher,
    length_minutes: title.length_minutes,
    published_at: title.published_at,
    categories: title.genres,
    language: title.language,
    is_abridged: title.is_abridged,
    showCommerce: true,
    ...extras,
  };
}

/**
 * Builds a {@link TitleDetail} from a library {@link BookRecord}.
 *
 * @param book - Library row.
 * @returns Detail model with commerce disabled.
 */
export function titleDetailFromBook(book: BookRecord): TitleDetail {
  return {
    title: book.title,
    subtitle: book.subtitle,
    authors: book.authors,
    narrators: book.narrators,
    series: book.series,
    series_index: book.series_index,
    asin: book.asin,
    isbn: book.isbn,
    cover_url: book.cover_url,
    library_uuid: book.uuid,
    description: book.description,
    publisher: book.publisher,
    length_minutes: book.length_minutes,
    published_at: book.published_at,
    categories: book.categories,
    tags: book.tags,
    language: book.language,
    is_abridged: book.is_abridged,
    rating_overall: book.rating_overall,
    rating_performance: book.rating_performance,
    rating_story: book.rating_story,
    acquire_status: book.acquire_status,
    source: book.source,
    marketplace: book.marketplace,
    account_id: book.account_id,
    product_id: book.product_id,
    content_kind: book.content_kind,
    storage_key: book.storage_key,
    error_message: book.error_message,
    created_at: book.created_at,
    updated_at: book.updated_at,
    showCommerce: false,
  };
}
