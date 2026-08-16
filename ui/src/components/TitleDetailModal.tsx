import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { Bookmark, Download, ExternalLink, Trash2, X } from "lucide-react";
import { StarRating } from "@/components/StarRating";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  coverUrl,
  fetchBooks,
  fetchPurchaseHints,
  fetchTitleMeta,
  fetchTitleReviews,
  type AcquireStatus,
  type BookRecord,
  type PurchaseHint,
  type StoreEdition,
  type TitleMeta,
  type TitleReview,
  type TitleReviewsSort,
  type TitleRequest,
} from "@/lib/api";
import {
  attachWishlistCommerce,
  descriptionLooksComplete,
  formatPurchasePrices,
  pickBetterDescription,
  splitMetaList,
  storeLabel,
  withAudibleEditionFromAsin,
  type CatalogTitle,
} from "@/lib/catalogTitle";
import { titleDetailFromCatalog, type TitleDetail } from "@/lib/titleDetail";
import { formatDate, formatDuration } from "@/lib/libraryFilters";
import { StoreLogo } from "@/components/StoreLogo";
import { WisherAvatars } from "@/components/WisherAvatars";
import {
  decodeHtmlEntities,
  parseGuidedReviewBody,
  prepareDescriptionHtml,
} from "@/lib/safeHtml";
import { cn, titleDetailDialogClass } from "@/lib/utils";

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

/**
 * Browse/search target for linked metadata in the detail dialog.
 */
export type TitleMetaSearchKind =
  | "authors"
  | "narrators"
  | "series"
  | "genres";

function MetaRow({ label, value }: { label: string; value: ReactNode }) {
  if (value == null || value === "" || value === "—") return null;
  return (
    <div className="grid grid-cols-[7.5rem_1fr] gap-x-3 gap-y-1 text-sm sm:grid-cols-[9rem_1fr]">
      <dt className="text-ink/50">{label}</dt>
      <dd className="min-w-0 break-words text-ink">{value}</dd>
    </div>
  );
}

function MetaSearchLink({
  kind,
  value,
  onSearch,
  children,
}: {
  kind: TitleMetaSearchKind;
  value: string;
  onSearch?: (kind: TitleMetaSearchKind, value: string) => void;
  children?: ReactNode;
}) {
  const label = children ?? value;
  if (!onSearch) {
    return <span>{label}</span>;
  }
  return (
    <button
      type="button"
      className="text-left font-medium text-teal hover:underline"
      onClick={() => onSearch(kind, value)}
    >
      {label}
    </button>
  );
}

function LinkedListRow({
  label,
  raw,
  kind,
  onSearch,
}: {
  label: string;
  raw: string | null | undefined;
  kind: TitleMetaSearchKind;
  onSearch?: (kind: TitleMetaSearchKind, value: string) => void;
}) {
  const items = splitMetaList(raw);
  if (items.length === 0) return null;
  return (
    <MetaRow
      label={label}
      value={
        <span className="inline">
          {items.map((item, i) => (
            <span key={`${kind}-${item}`}>
              {i > 0 ? <span className="text-ink/40">, </span> : null}
              <MetaSearchLink kind={kind} value={item} onSearch={onSearch}>
                {item}
              </MetaSearchLink>
            </span>
          ))}
        </span>
      }
    />
  );
}

function SeriesRow({
  series,
  seriesIndex,
  onSearch,
}: {
  series: string | null | undefined;
  seriesIndex: string | null | undefined;
  onSearch?: (kind: TitleMetaSearchKind, value: string) => void;
}) {
  const name = series?.trim();
  if (!name) return null;
  const index = seriesIndex?.trim();
  return (
    <MetaRow
      label="Series"
      value={
        <span>
          <MetaSearchLink kind="series" value={name} onSearch={onSearch}>
            {name}
          </MetaSearchLink>
          {index ? <span className="text-ink/60"> #{index}</span> : null}
        </span>
      }
    />
  );
}

function GenreTagsRow({
  raw,
  onSearch,
}: {
  raw: string | null | undefined;
  onSearch?: (kind: TitleMetaSearchKind, value: string) => void;
}) {
  const genres = splitMetaList(raw);
  if (genres.length === 0) return null;
  return (
    <MetaRow
      label="Genres"
      value={
        <div className="flex flex-wrap gap-1.5">
          {genres.map((genre) =>
            onSearch ? (
              <button
                key={genre}
                type="button"
                onClick={() => onSearch("genres", genre)}
                className="inline-flex items-center rounded-md bg-ink/8 px-2 py-0.5 text-xs font-medium normal-case tracking-normal text-ink/75 transition-colors hover:bg-teal/15 hover:text-teal"
              >
                {genre}
              </button>
            ) : (
              <span
                key={genre}
                className="inline-flex items-center rounded-md bg-ink/8 px-2 py-0.5 text-xs font-medium normal-case tracking-normal text-ink/75"
              >
                {genre}
              </span>
            ),
          )}
        </div>
      }
    />
  );
}

function coversFor(detail: TitleDetail): string[] {
  const out: string[] = [];
  if (detail.library_uuid) out.push(coverUrl(detail.library_uuid));
  const remote = detail.cover_url?.trim();
  if (remote) out.push(absoluteCoverUrl(remote));
  out.push(FALLBACK_COVER);
  return out;
}

function mergeLibraryBook(detail: TitleDetail, book: BookRecord): TitleDetail {
  return {
    ...detail,
    subtitle: detail.subtitle ?? book.subtitle,
    narrators: detail.narrators ?? book.narrators,
    series: detail.series ?? book.series,
    series_index: detail.series_index ?? book.series_index,
    description: pickBetterDescription(detail.description, book.description),
    publisher: detail.publisher ?? book.publisher,
    length_minutes: detail.length_minutes ?? book.length_minutes,
    published_at: detail.published_at ?? book.published_at,
    categories: detail.categories ?? book.categories,
    tags: detail.tags ?? book.tags,
    language: detail.language ?? book.language,
    is_abridged: detail.is_abridged ?? book.is_abridged,
    rating_overall: detail.rating_overall ?? book.rating_overall,
    rating_performance: detail.rating_performance ?? book.rating_performance,
    rating_story: detail.rating_story ?? book.rating_story,
    cover_url: detail.cover_url ?? book.cover_url,
    library_uuid: detail.library_uuid ?? book.uuid,
    asin: detail.asin ?? book.asin,
    isbn: detail.isbn ?? book.isbn,
  };
}

function nonempty(v: string | null | undefined): string | null {
  const t = v?.trim();
  return t ? v! : null;
}

/** Wishlist status is already shown as the teal badge — hide duplicate reasons. */
function displayReasons(reasons: string[] | null | undefined): string[] {
  return (reasons ?? []).filter((r) => {
    const t = r.trim().toLowerCase();
    return t !== "on the wishlist" && !/^wishlisted by \d+/.test(t);
  });
}

function mergePublicMeta(detail: TitleDetail, meta: TitleMeta): TitleDetail {
  const asin = nonempty(detail.asin) ?? meta.asin;
  const isbn = nonempty(detail.isbn) ?? meta.isbn;
  return withAudibleEditionFromAsin({
    ...detail,
    subtitle: nonempty(detail.subtitle) ?? meta.subtitle,
    authors: nonempty(detail.authors) ?? meta.authors,
    narrators: nonempty(detail.narrators) ?? meta.narrators,
    series: nonempty(detail.series) ?? meta.series,
    series_index: nonempty(detail.series_index) ?? meta.series_index,
    description: pickBetterDescription(detail.description, meta.description),
    publisher: nonempty(detail.publisher) ?? meta.publisher,
    length_minutes: detail.length_minutes ?? meta.length_minutes,
    published_at: nonempty(detail.published_at) ?? meta.published_at,
    categories: nonempty(detail.categories) ?? meta.categories,
    language: nonempty(detail.language) ?? meta.language,
    is_abridged: detail.is_abridged ?? meta.is_abridged ?? null,
    cover_url: nonempty(detail.cover_url) ?? meta.cover_url,
    asin,
    isbn,
    rating_overall: detail.rating_overall ?? meta.rating_overall ?? null,
    rating_performance: detail.rating_performance ?? meta.rating_performance ?? null,
    rating_story: detail.rating_story ?? meta.rating_story ?? null,
    rating_count: detail.rating_count ?? meta.rating_count ?? null,
    review_count: detail.review_count ?? meta.review_count ?? null,
    reviews:
      detail.reviews && detail.reviews.length > 0
        ? detail.reviews
        : meta.reviews && meta.reviews.length > 0
          ? meta.reviews
          : detail.reviews,
  });
}

function formatCount(n: number): string {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 }).format(n);
}

function detailIdentityKey(d: TitleDetail): string {
  return [
    d.work_key ?? "",
    d.library_uuid ?? "",
    d.asin ?? "",
    d.isbn ?? "",
    d.title,
  ].join("|");
}

function ReviewBlock({ review }: { review: TitleReview }) {
  const [expanded, setExpanded] = useState(false);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const [needsTruncate, setNeedsTruncate] = useState(false);
  const guided = parseGuidedReviewBody(review.body);
  const html = guided ? "" : prepareDescriptionHtml(review.body);
  const title = review.title?.trim()
    ? decodeHtmlEntities(review.title.trim())
    : null;
  const author = review.author_name?.trim()
    ? decodeHtmlEntities(review.author_name.trim())
    : null;

  const clampClass = guided ? "line-clamp-6" : "line-clamp-4";

  useLayoutEffect(() => {
    const el = bodyRef.current;
    if (!el || expanded) return;
    el.classList.remove(clampClass);
    const fullHeight = el.scrollHeight;
    el.classList.add(clampClass);
    setNeedsTruncate(fullHeight > el.clientHeight + 2);
  }, [html, guided, expanded, clampClass]);

  const submitted = review.submitted_at ? formatDate(review.submitted_at) : null;

  return (
    <div className="space-y-1">
      {title ? (
        <p className="text-sm font-semibold text-ink">{title}</p>
      ) : null}
      {review.overall_rating != null || author || submitted ? (
        <p className="flex flex-wrap items-center gap-x-1.5 text-xs text-ink/50">
          {review.overall_rating != null ? (
            <StarRating value={review.overall_rating} />
          ) : null}
          {author ? <span>· {author}</span> : null}
          {submitted ? <span>· {submitted}</span> : null}
        </p>
      ) : null}
      {guided ? (
        <div
          ref={bodyRef}
          className={cn(
            "space-y-3 rounded-md border border-teal/25 bg-teal/5 px-2.5 py-2 text-sm leading-relaxed text-ink/80",
            !expanded && clampClass,
          )}
        >
          {guided.map((section, i) => {
            const answerHtml = prepareDescriptionHtml(section.answer);
            return (
              <div key={`${section.type}-${i}`} className="space-y-1">
                {section.question ? (
                  <p className="text-[13px] font-medium leading-snug text-teal">
                    {section.question}
                  </p>
                ) : section.type ? (
                  <p className="text-[13px] font-medium leading-snug text-teal">
                    {section.type}
                  </p>
                ) : null}
                {answerHtml ? (
                  <div
                    className="prose-detail text-ink/80"
                    dangerouslySetInnerHTML={{ __html: answerHtml }}
                  />
                ) : null}
              </div>
            );
          })}
        </div>
      ) : (
        <div
          ref={bodyRef}
          className={cn(
            "prose-detail text-sm leading-relaxed text-ink/80",
            !expanded && clampClass,
          )}
          dangerouslySetInnerHTML={{ __html: html }}
        />
      )}
      {needsTruncate || expanded ? (
        <button
          type="button"
          className="text-sm font-semibold text-teal hover:underline"
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded ? "Show less" : "Read more"}
        </button>
      ) : null}
    </div>
  );
}

function DescriptionBlock({
  text,
  expanded,
  onToggle,
}: {
  text: string;
  expanded: boolean;
  onToggle: () => void;
}) {
  const html = prepareDescriptionHtml(text);
  const bodyRef = useRef<HTMLDivElement | null>(null);
  const [needsTruncate, setNeedsTruncate] = useState(false);

  useLayoutEffect(() => {
    const el = bodyRef.current;
    if (!el || expanded) return;
    // `-webkit-line-clamp` often reports scrollHeight == clientHeight while
    // clipped; measure natural height with the clamp temporarily removed.
    el.classList.remove("line-clamp-6");
    const fullHeight = el.scrollHeight;
    el.classList.add("line-clamp-6");
    const clampedHeight = el.clientHeight;
    setNeedsTruncate(fullHeight > clampedHeight + 1);
  }, [html, expanded]);

  return (
    <div className="space-y-2">
      <div
        ref={bodyRef}
        className={cn(
          "title-description text-sm leading-relaxed text-ink/80 [&_b]:font-semibold [&_strong]:font-semibold [&_p]:mb-2 [&_p:last-child]:mb-0 [&_ul]:my-2 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:my-2 [&_ol]:list-decimal [&_ol]:pl-5",
          !expanded ? "line-clamp-6" : null,
        )}
        // Safe: prepareDescriptionHtml escapes text and emits only allowlisted tags.
        dangerouslySetInnerHTML={{ __html: html }}
      />
      {needsTruncate ? (
        <button
          type="button"
          onClick={onToggle}
          className="text-sm font-semibold text-teal hover:underline"
        >
          {expanded ? "Show less" : "Read more"}
        </button>
      ) : null}
    </div>
  );
}

/**
 * Rich title detail dialog (covers, meta links, wishlist/acquire, reviews).
 *
 * @param props - Detail model, busy flag, close/wishlist/acquire/search handlers.
 */
export function TitleDetailModal({
  detail: initial,
  busy = false,
  onClose,
  onWishlist,
  onRemoveWishlist,
  onAcquire,
  onMetaSearch,
  showAcquire = false,
}: {
  detail: TitleDetail;
  busy?: boolean;
  onClose: () => void;
  onWishlist?: (detail: TitleDetail) => void;
  onRemoveWishlist?: (detail: TitleDetail) => void;
  onAcquire?: (detail: TitleDetail) => void;
  /** Open an author / narrator / series / genre search on the parent surface. */
  onMetaSearch?: (kind: TitleMetaSearchKind, value: string) => void;
  showAcquire?: boolean;
}) {
  const [detail, setDetail] = useState(initial);
  const [hints, setHints] = useState<PurchaseHint[]>(initial.purchase_hints ?? []);
  const [hintsBusy, setHintsBusy] = useState(false);
  const [descExpanded, setDescExpanded] = useState(false);
  const [reviews, setReviews] = useState<TitleReview[]>([]);
  const [reviewsPage, setReviewsPage] = useState(0);
  const [reviewsSort, setReviewsSort] =
    useState<TitleReviewsSort>("MostHelpful");
  const [reviewsHasMore, setReviewsHasMore] = useState(false);
  const [reviewsLoading, setReviewsLoading] = useState(false);
  const scrollBodyRef = useRef<HTMLDivElement | null>(null);
  const reviewsSentinelRef = useRef<HTMLDivElement | null>(null);
  const reviewsLoadingRef = useRef(false);
  const reviewsSortRef = useRef<TitleReviewsSort>("MostHelpful");
  const candidates = coversFor(detail);
  const [coverIndex, setCoverIndex] = useState(0);
  const src = candidates[Math.min(coverIndex, candidates.length - 1)]!;
  const isFallback = src === FALLBACK_COVER;
  const wishlisted = Boolean(detail.wishlist_uuid);
  const showCommerce = detail.showCommerce !== false;

  const identityKey = detailIdentityKey(initial);
  const reviewAsin = (detail.asin ?? initial.asin)?.trim() || "";

  useEffect(() => {
    setDetail(initial);
    setHints(initial.purchase_hints ?? []);
    setCoverIndex(0);
    setDescExpanded(false);
    setReviews([]);
    setReviewsPage(0);
    setReviewsSort("MostHelpful");
    reviewsSortRef.current = "MostHelpful";
    setReviewsHasMore(false);
    setReviewsLoading(false);
    // Parents often recreate the detail object each render; identityKey is the
    // stable open-title gate (and pages also remount via `key`).
  }, [identityKey]); // eslint-disable-line react-hooks/exhaustive-deps -- intentional

  // Wishlist add/remove (and household wisher stacks) update the parent prop
  // without changing identityKey. Fingerprint avoids re-copying on every
  // parent render of a new `detail` object.
  const wishlistSyncKey = `${initial.wishlist_uuid ?? ""}:${initial.wish_count ?? ""}:${(initial.wishers ?? [])
    .map((wisher) => wisher.user_id ?? wisher.identity_id ?? "")
    .join(",")}`;
  useEffect(() => {
    setDetail((d) => ({
      ...d,
      wishlist_uuid: initial.wishlist_uuid ?? null,
      wishers: initial.wishers,
      wish_count: initial.wish_count,
    }));
  }, [wishlistSyncKey]); // eslint-disable-line react-hooks/exhaustive-deps -- fingerprint is the gate

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

  // Hydrate missing bibliographic fields from the local library when possible.
  useEffect(() => {
    const q = (initial.asin || initial.isbn || initial.title || "").trim();
    if (!q || descriptionLooksComplete(initial.description)) return;
    let cancelled = false;
    void (async () => {
      try {
        const res = await fetchBooks({ q, limit: 8 });
        if (cancelled) return;
        const needleAsin = initial.asin?.toUpperCase();
        const needleIsbn = initial.isbn?.replace(/[^0-9Xx]/g, "").toUpperCase();
        const match =
          res.books.find(
            (b) =>
              (needleAsin && b.asin?.toUpperCase() === needleAsin) ||
              (needleIsbn &&
                b.isbn?.replace(/[^0-9Xx]/g, "").toUpperCase() === needleIsbn),
          ) ??
          res.books.find(
            (b) =>
              b.title.trim().toLowerCase() === initial.title.trim().toLowerCase(),
          );
        if (match) setDetail((d) => mergeLibraryBook(d, match));
      } catch {
        // optional enrichment
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [identityKey, initial.asin, initial.isbn, initial.title, initial.description]);

  // Public Audnexus / catalog blurb, runtime, publisher, ratings (all detail surfaces).
  useEffect(() => {
    const hasBlurb = descriptionLooksComplete(initial.description);
    const hasRuntime = initial.length_minutes != null;
    const hasPublisher = Boolean(initial.publisher?.trim());
    const hasNarrators = Boolean(initial.narrators?.trim());
    const hasGenres = Boolean(initial.categories?.trim());
    const hasRatings = initial.rating_overall != null;
    const hasAsin = Boolean(initial.asin?.trim());
    if (
      hasBlurb &&
      hasRuntime &&
      hasPublisher &&
      hasNarrators &&
      hasGenres &&
      hasRatings &&
      hasAsin
    ) {
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const meta = await fetchTitleMeta({
          title: initial.title,
          authors: initial.authors,
          asin: initial.asin,
          isbn: initial.isbn,
          narrators: initial.narrators,
          length_minutes: initial.length_minutes,
        });
        if (cancelled || !meta) return;
        setDetail((d) => mergePublicMeta(d, meta));
      } catch {
        // optional
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [
    identityKey,
    initial.title,
    initial.authors,
    initial.asin,
    initial.isbn,
    initial.narrators,
    initial.length_minutes,
    initial.description,
    initial.publisher,
    initial.categories,
    initial.rating_overall,
  ]);

  // Paginated Audible reviews: seed MostHelpful, then continue MostRecent
  // (Roger Baer / guided questionnaire reviews only appear in MostRecent).
  useEffect(() => {
    if (!reviewAsin) {
      setReviews([]);
      setReviewsPage(0);
      setReviewsHasMore(false);
      return;
    }
    let cancelled = false;
    reviewsLoadingRef.current = true;
    reviewsSortRef.current = "MostHelpful";
    setReviewsSort("MostHelpful");
    setReviewsLoading(true);
    void (async () => {
      try {
        const page = await fetchTitleReviews({
          asin: reviewAsin,
          page: 1,
          page_size: 20,
          sort_by: "MostHelpful",
        });
        if (cancelled) return;
        setReviews(page.reviews);
        // After the helpful seed, scroll continues in MostRecent.
        reviewsSortRef.current = "MostRecent";
        setReviewsSort("MostRecent");
        setReviewsPage(0);
        setReviewsHasMore(true);
      } catch {
        if (!cancelled) {
          setReviews([]);
          setReviewsHasMore(false);
        }
      } finally {
        reviewsLoadingRef.current = false;
        if (!cancelled) setReviewsLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [identityKey, reviewAsin]);

  useEffect(() => {
    const root = scrollBodyRef.current;
    const sentinel = reviewsSentinelRef.current;
    if (!root || !sentinel || !reviewAsin || !reviewsHasMore) return;

    const obs = new IntersectionObserver(
      (entries) => {
        if (!entries.some((e) => e.isIntersecting)) return;
        if (reviewsLoadingRef.current || !reviewsHasMore) return;
        const sort = reviewsSortRef.current;
        const nextPage = reviewsPage + 1;
        reviewsLoadingRef.current = true;
        setReviewsLoading(true);
        void (async () => {
          try {
            const page = await fetchTitleReviews({
              asin: reviewAsin,
              page: nextPage,
              page_size: 20,
              sort_by: sort,
            });
            setReviews((prev) => {
              const seen = new Set(
                prev.map(
                  (r) =>
                    r.id?.trim() ||
                    `${r.author_name}|${r.title}|${r.body}`,
                ),
              );
              const merged = [...prev];
              let added = 0;
              for (const review of page.reviews) {
                const key =
                  review.id?.trim() ||
                  `${review.author_name}|${review.title}|${review.body}`;
                if (seen.has(key)) continue;
                seen.add(key);
                merged.push(review);
                added += 1;
              }
              // Keep paging while Audible reports more, even if this chunk was
              // all duplicates of the MostHelpful seed.
              setReviewsHasMore(page.has_more);
              return merged;
            });
            setReviewsPage(page.page);
          } catch {
            setReviewsHasMore(false);
          } finally {
            reviewsLoadingRef.current = false;
            setReviewsLoading(false);
          }
        })();
      },
      { root, rootMargin: "120px" },
    );
    obs.observe(sentinel);
    return () => obs.disconnect();
  }, [identityKey, reviewAsin, reviewsHasMore, reviewsPage, reviewsSort]);

  // Commerce ids may arrive later from title-meta (ASIN/ISBN gap-fill).
  const commerceAsin = (detail.asin ?? initial.asin)?.trim() || undefined;
  const commerceIsbn = (detail.isbn ?? initial.isbn)?.trim() || undefined;
  const commerceEditions = detail.store_editions ?? initial.store_editions;
  const commerceEditionsKey = JSON.stringify(commerceEditions ?? []);
  const commerceSourcesKey = JSON.stringify(detail.sources ?? []);

  // Live store links + pricing for Discover / Wishlist.
  useEffect(() => {
    if (!showCommerce) return;
    const editions = JSON.parse(commerceEditionsKey) as StoreEdition[];
    const sources = JSON.parse(commerceSourcesKey) as string[];
    let cancelled = false;
    setHintsBusy(true);
    void (async () => {
      try {
        const primaryEdition =
          editions.find((e) =>
            sources.some((s) => s.toLowerCase() === e.source.toLowerCase()),
          ) ?? editions[0];
        const res = await fetchPurchaseHints({
          title: initial.title,
          authors: initial.authors ?? detail.authors,
          asin: commerceAsin,
          isbn: commerceIsbn,
          store_editions: editions,
          candidate_source: primaryEdition?.source,
          candidate_product_id: primaryEdition?.product_id,
        });
        if (cancelled) return;
        setHints(res.hints);
      } catch {
        // keep seeds
      } finally {
        if (!cancelled) setHintsBusy(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [
    identityKey,
    showCommerce,
    initial.title,
    initial.authors,
    detail.authors,
    commerceAsin,
    commerceIsbn,
    // Serialize editions/sources so parent array identity churn does not refetch.
    commerceEditionsKey,
    commerceSourcesKey,
  ]);

  // Keep known catalog editions visible in Where-to-buy while priced resolve
  // catches up (and if a store is briefly missing from the API response).
  const commerceHints = (() => {
    const out = [...hints];
    const have = new Set(out.map((h) => h.source.toLowerCase()));
    const seedUrl = (source: string, productId: string): string | null => {
      const pid = productId.trim();
      if (!pid) return null;
      switch (source.toLowerCase()) {
        case "audible":
          return `https://www.audible.com/pd/${encodeURIComponent(pid)}`;
        case "libro":
          return `https://libro.fm/audiobooks/${encodeURIComponent(pid)}`;
        case "chirp":
          return `https://www.chirpbooks.com/audiobooks/${encodeURIComponent(pid)}`;
        case "graphicaudio":
          return `https://www.graphicaudio.net/catalog/product/view/id/${encodeURIComponent(pid)}`;
        default:
          return null;
      }
    };
    const pushSeed = (source: string, productId: string) => {
      const key = source.toLowerCase();
      if (have.has(key)) return;
      const url = seedUrl(source, productId);
      if (!url) return;
      have.add(key);
      out.push({
        source: key,
        product_id: productId.trim(),
        title: detail.title,
        url,
      });
    };
    for (const e of commerceEditions ?? []) {
      pushSeed(e.source, e.product_id);
    }
    if (commerceAsin) pushSeed("audible", commerceAsin);
    return out;
  })();

  // Union known catalog editions with live hints. Do not replace search
  // sources with a partial hint set (e.g. Audible-only while Chirp is still
  // resolving). Dropping a store still happens when it is absent from both
  // editions and the final hint list.
  const sourceBadges = (() => {
    const set = new Set<string>();
    for (const s of detail.sources ?? []) {
      if (s.trim()) set.add(s.trim().toLowerCase());
    }
    for (const e of detail.store_editions ?? []) {
      if (e.source.trim()) set.add(e.source.trim().toLowerCase());
    }
    if (showCommerce) {
      for (const h of commerceHints) {
        if (h.source.trim()) set.add(h.source.trim().toLowerCase());
      }
    }
    if (commerceAsin) set.add("audible");
    return [...set];
  })();

  return (
    <div
      className="fixed inset-0 z-50 flex items-end justify-center bg-scrim p-0 backdrop-blur-[2px] sm:items-center sm:p-6 lg:p-8"
      role="presentation"
      onClick={onClose}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="title-detail-heading"
        className={cn(
          "animate-[detailIn_220ms_ease-out] flex max-h-[92vh] flex-col overflow-hidden rounded-t-xl bg-paper shadow-xl sm:rounded-xl",
          titleDetailDialogClass,
        )}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-start justify-between gap-3 border-b border-ink/10 px-4 py-3 sm:px-6 lg:px-8">
          <div className="min-w-0">
            <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
              Title details
            </p>
            <h2
              id="title-detail-heading"
              className="mt-0.5 font-display text-xl font-bold leading-tight text-ink sm:text-2xl"
            >
              {detail.title}
            </h2>
            {detail.subtitle ? (
              <p className="mt-1 text-sm text-ink/65">{detail.subtitle}</p>
            ) : null}
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

        <div
          ref={scrollBodyRef}
          className="overflow-y-auto px-4 py-4 sm:px-6 sm:py-5 lg:px-8"
        >
          <div className="flex flex-col gap-5 sm:flex-row sm:gap-6 lg:gap-8">
            <div className="mx-auto aspect-square h-44 w-44 shrink-0 overflow-hidden rounded-md bg-fold shadow-md sm:mx-0 sm:h-52 sm:w-52 lg:h-60 lg:w-60">
              <img
                src={src}
                alt={isFallback ? "" : `Cover for ${detail.title}`}
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
                {detail.acquire_status ? (
                  <Badge className={cn(statusStyles[detail.acquire_status])}>
                    {detail.acquire_status.replaceAll("_", " ")}
                  </Badge>
                ) : null}
                {wishlisted ? (
                  <Badge className="bg-teal/15 text-teal">On wishlist</Badge>
                ) : null}
                {detail.source ? (
                  <Badge className="bg-ink/8 text-ink/70">{detail.source}</Badge>
                ) : null}
                {sourceBadges.map((s) => (
                  <Badge key={s} className="bg-ink/8 text-ink/70">
                    {storeLabel(s)}
                  </Badge>
                ))}
              </div>
              {(detail.wishers?.length ?? 0) > 0 ? (
                <div className="flex flex-wrap items-center gap-2">
                  <WisherAvatars
                    wishers={detail.wishers ?? []}
                    wishCount={detail.wish_count}
                    max={8}
                    avatarClassName="h-7 w-7 text-[11px]"
                  />
                  <p className="text-sm text-ink/60">
                    Wishlisted by{" "}
                    <span className="tabular-nums font-medium text-ink">
                      {detail.wish_count ?? detail.wishers?.length ?? 0}
                    </span>
                    {(detail.wish_count ?? detail.wishers?.length ?? 0) === 1
                      ? " person"
                      : " people"}
                  </p>
                </div>
              ) : null}
              <dl className="space-y-2">
                <LinkedListRow
                  label="Authors"
                  raw={detail.authors}
                  kind="authors"
                  onSearch={onMetaSearch}
                />
                <LinkedListRow
                  label="Narrators"
                  raw={detail.narrators}
                  kind="narrators"
                  onSearch={onMetaSearch}
                />
                <SeriesRow
                  series={detail.series}
                  seriesIndex={detail.series_index}
                  onSearch={onMetaSearch}
                />
                <MetaRow label="Publisher" value={detail.publisher} />
                <MetaRow
                  label="Duration"
                  value={formatDuration(detail.length_minutes ?? null)}
                />
                <MetaRow label="Published" value={formatDate(detail.published_at ?? null)} />
                <MetaRow
                  label="Format"
                  value={
                    detail.is_abridged == null
                      ? null
                      : detail.is_abridged
                        ? "Abridged"
                        : "Unabridged"
                  }
                />
                <GenreTagsRow raw={detail.categories} onSearch={onMetaSearch} />
                <MetaRow label="Tags" value={detail.tags} />
                <MetaRow label="Language" value={detail.language} />
                <MetaRow label="Kind" value={detail.content_kind} />
                <MetaRow label="Marketplace" value={detail.marketplace} />
                <MetaRow label="Account" value={detail.account_id} />
                <MetaRow label="ASIN" value={detail.asin} />
                <MetaRow label="ISBN" value={detail.isbn} />
                <MetaRow label="Product ID" value={detail.product_id} />
                <MetaRow label="Storage" value={detail.storage_key} />
                <MetaRow label="Notes" value={detail.notes} />
                <MetaRow label="Added" value={formatDate(detail.created_at ?? null)} />
                <MetaRow label="Updated" value={formatDate(detail.updated_at ?? null)} />
              </dl>
              {(() => {
                const reason = displayReasons(detail.reasons)[0];
                return reason ? (
                  <p className="rounded-md bg-ink/5 px-3 py-2 text-sm text-ink/70">
                    {reason}
                  </p>
                ) : null;
              })()}
              {detail.error_message ? (
                <p className="rounded-md bg-brick/10 px-3 py-2 text-sm text-brick">
                  {detail.error_message}
                </p>
              ) : null}
            </div>
          </div>

          {showCommerce ? (
            <div className="mt-5 space-y-2 border-t border-ink/10 pt-4">
              <div className="flex items-baseline justify-between gap-2">
                <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Where to buy
                </p>
                {hintsBusy ? (
                  <span className="text-[11px] text-ink/40">Checking stores…</span>
                ) : null}
              </div>
              {commerceHints.length === 0 && !hintsBusy ? (
                <p className="text-sm text-ink/50">No verified storefront matches yet.</p>
              ) : (
                <ul className="space-y-2">
                  {commerceHints.map((h) => (
                    <li
                      key={`${h.source}-${h.product_id}`}
                      className="flex flex-wrap items-center justify-between gap-2 rounded-md bg-card-mid px-3 py-2 ring-1 ring-ink/5"
                    >
                      <div className="flex min-w-0 items-start gap-2.5">
                        <StoreLogo source={h.source} className="mt-0.5" />
                        <div className="min-w-0">
                          <p className="text-sm font-semibold text-ink">
                            {storeLabel(h.source)}
                            {(() => {
                              const price = formatPurchasePrices(h);
                              return price ? (
                                <span className="ml-2 tabular-nums text-teal">
                                  {price}
                                </span>
                              ) : null;
                            })()}
                          </p>
                          {h.title ? (
                            <p className="truncate text-xs text-ink/50">{h.title}</p>
                          ) : null}
                        </div>
                      </div>
                      {h.url ? (
                        <a
                          href={h.url}
                          target="_blank"
                          rel="noreferrer"
                          className="inline-flex shrink-0 items-center gap-1 text-sm font-semibold text-teal hover:underline"
                        >
                          <ExternalLink className="h-3.5 w-3.5" />
                          Open
                        </a>
                      ) : null}
                    </li>
                  ))}
                </ul>
              )}
            </div>
          ) : null}

          {detail.description ? (
            <div className="mt-5 space-y-1.5 border-t border-ink/10 pt-4">
              <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                Description
              </p>
              <DescriptionBlock
                text={detail.description}
                expanded={descExpanded}
                onToggle={() => setDescExpanded((v) => !v)}
              />
            </div>
          ) : null}

          {detail.rating_overall != null ||
          detail.rating_count != null ||
          detail.review_count != null ||
          reviews.length > 0 ||
          (Boolean(reviewAsin) && reviewsLoading) ? (
            <div
              className={cn(
                "space-y-3",
                detail.description
                  ? "mt-4"
                  : "mt-5 border-t border-ink/10 pt-4",
              )}
            >
              <div className="space-y-1.5">
                <p className="text-xs font-semibold uppercase tracking-wide text-ink/45">
                  Ratings & reviews
                </p>
                {detail.rating_overall != null ? (
                  <p className="text-sm text-ink">
                    <span className="tabular-nums font-semibold text-teal">
                      {detail.rating_overall.toFixed(1)}
                    </span>
                    <span className="text-ink/70"> overall</span>
                    {detail.rating_performance != null ? (
                      <span className="text-ink/70">
                        {" "}
                        ·{" "}
                        <span className="tabular-nums text-ink">
                          {detail.rating_performance.toFixed(1)}
                        </span>{" "}
                        performance
                      </span>
                    ) : null}
                    {detail.rating_story != null ? (
                      <span className="text-ink/70">
                        {" "}
                        ·{" "}
                        <span className="tabular-nums text-ink">
                          {detail.rating_story.toFixed(1)}
                        </span>{" "}
                        story
                      </span>
                    ) : null}
                  </p>
                ) : null}
                {detail.rating_count != null || detail.review_count != null ? (
                  <p className="text-xs text-ink/50">
                    {[
                      detail.rating_count != null
                        ? `${formatCount(detail.rating_count)} ratings`
                        : null,
                      detail.review_count != null
                        ? `${formatCount(detail.review_count)} reviews`
                        : null,
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                    {" · Audible"}
                  </p>
                ) : (
                  <p className="text-xs text-ink/50">Audible community reviews</p>
                )}
              </div>
              {reviews.length > 0 ? (
                <ul className="space-y-3">
                  {reviews.map((review, idx) => (
                    <li
                      key={review.id ?? `${review.author_name ?? "review"}-${idx}`}
                      className="rounded-md bg-card-mid px-3 py-2.5 ring-1 ring-ink/5"
                    >
                      <ReviewBlock review={review} />
                    </li>
                  ))}
                </ul>
              ) : reviewsLoading ? (
                <p className="text-xs text-ink/45">Loading reviews…</p>
              ) : null}
              {reviewAsin && reviewsHasMore ? (
                <div
                  ref={reviewsSentinelRef}
                  className="flex h-8 items-center justify-center text-xs text-ink/40"
                  aria-hidden
                >
                  {reviewsLoading ? "Loading more…" : null}
                </div>
              ) : null}
            </div>
          ) : null}
        </div>

        <div className="flex flex-wrap items-center justify-end gap-2 border-t border-ink/10 px-4 py-3 sm:px-6 lg:px-8">
          <Button variant="ghost" onClick={onClose}>
            Close
          </Button>
          {wishlisted && onRemoveWishlist ? (
            <Button
              variant="secondary"
              disabled={busy}
              onClick={() => onRemoveWishlist(detail)}
            >
              <Trash2 className="h-4 w-4" />
              Remove
            </Button>
          ) : null}
          {!wishlisted && onWishlist ? (
            <Button disabled={busy} onClick={() => onWishlist(detail)}>
              <Bookmark className="h-4 w-4" />
              Wishlist
            </Button>
          ) : null}
          {wishlisted && !onRemoveWishlist ? (
            <Button variant="secondary" disabled>
              <Bookmark className="h-4 w-4 fill-current" />
              Wishlisted
            </Button>
          ) : null}
          {showAcquire &&
          onAcquire &&
          detail.acquire_status &&
          detail.acquire_status !== "acquired" ? (
            <Button
              onClick={() => onAcquire(detail)}
              disabled={busy || detail.acquire_status === "downloading"}
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

type CatalogTitleDetailModalProps = {
  title: CatalogTitle;
  /** Personal wishlist rows whose snapshotted stores should appear in Where-to-buy. */
  wishlist?: TitleRequest[];
  busy?: boolean;
  onClose: () => void;
  onWishlist?: (detail: TitleDetail) => void;
  onRemoveWishlist?: (detail: TitleDetail) => void;
  onMetaSearch?: (kind: TitleMetaSearchKind, value: string) => void;
};

/**
 * Shared Discover / Wishlist title dialog. Hydrates store editions from the
 * matching wishlist snapshot so both surfaces show the same Where-to-buy list.
 *
 * @param props - Catalog title, optional wishlist rows, and dialog handlers.
 */
export function CatalogTitleDetailModal({
  title,
  wishlist = [],
  busy,
  onClose,
  onWishlist,
  onRemoveWishlist,
  onMetaSearch,
}: CatalogTitleDetailModalProps) {
  const hydrated = attachWishlistCommerce(title, wishlist);
  const commerceKey = (hydrated.store_editions ?? [])
    .map((edition) => `${edition.source}:${edition.product_id}`)
    .sort()
    .join("|");
  return (
    <TitleDetailModal
      key={`${hydrated.work_key}|${hydrated.wishlist_uuid ?? ""}|${commerceKey}`}
      detail={titleDetailFromCatalog(hydrated)}
      busy={busy}
      onClose={onClose}
      onWishlist={onWishlist}
      onRemoveWishlist={onRemoveWishlist}
      onMetaSearch={onMetaSearch}
    />
  );
}
