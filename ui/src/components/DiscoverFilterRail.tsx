import { useMemo, useState, type ReactNode } from "react";
import { Ban } from "lucide-react";
import { StoreLogo } from "@/components/StoreLogo";
import {
  RUNTIME_BUCKET_OPTIONS,
  storeLabel,
  type RuntimeBucket,
} from "@/lib/catalogTitle";
import { cn } from "@/lib/utils";

/**
 * Value/label pair for Discover filter checklists.
 */
export type FacetOption = { value: string; label: string };

function FilterSection({
  title,
  children,
}: {
  title: string;
  children: ReactNode;
}) {
  return (
    <section className="space-y-2 border-b border-ink/10 pb-4 last:border-b-0 last:pb-0">
      <h3 className="text-xs font-semibold uppercase tracking-wide text-ink/55">
        {title}
      </h3>
      {children}
    </section>
  );
}

function SearchableCheckList({
  options,
  selected,
  onChange,
  placeholder,
  emptyLabel,
}: {
  options: FacetOption[];
  selected: string[];
  onChange: (next: string[]) => void;
  placeholder: string;
  emptyLabel: string;
}) {
  const [q, setQ] = useState("");
  const needle = q.trim().toLowerCase();
  const visible = useMemo(() => {
    const base = needle
      ? options.filter(
          (o) =>
            o.label.toLowerCase().includes(needle) ||
            o.value.toLowerCase().includes(needle),
        )
      : options;
    // Keep selected values visible even if they fall outside the typeahead.
    const selectedMissing = selected
      .filter((v) => !base.some((o) => o.value === v))
      .map((v) => ({ value: v, label: v }));
    return [...selectedMissing, ...base];
  }, [options, selected, needle]);

  function toggle(value: string) {
    if (selected.includes(value)) {
      onChange(selected.filter((v) => v !== value));
    } else {
      onChange([...selected, value]);
    }
  }

  return (
    <div className="space-y-2">
      <input
        type="search"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        placeholder={placeholder}
        className="w-full rounded-md border border-ink/15 bg-card-strong px-2 py-1.5 text-sm shadow-sm focus:border-teal focus:outline-none focus:ring-2 focus:ring-teal/30"
      />
      <ul className="space-y-1 text-sm">
        {visible.length === 0 ? (
          <li className="text-ink/45">{emptyLabel}</li>
        ) : (
          visible.map((opt) => (
            <li key={opt.value}>
              <label className="flex cursor-pointer items-start gap-2 text-ink">
                <input
                  type="checkbox"
                  className="mt-0.5"
                  checked={selected.includes(opt.value)}
                  onChange={() => toggle(opt.value)}
                />
                <span className="leading-snug">{opt.label}</span>
              </label>
            </li>
          ))
        )}
      </ul>
    </div>
  );
}

/**
 * Discover results filter rail (authors, narrators, series, genres, sources, …).
 *
 * @param props - Facet options, selected filters, and change handlers.
 */
export function DiscoverFilterRail({
  className,
  authorOptions,
  narratorOptions,
  seriesOptions,
  genreOptions,
  sourceOptions,
  filterAuthors,
  filterNarrators,
  filterSeries,
  filterGenres,
  excludedSources,
  enabledSourceIds,
  minRating,
  runtimeBucket,
  onAuthorsChange,
  onNarratorsChange,
  onSeriesChange,
  onGenresChange,
  onExcludedSourcesChange,
  onMinRatingChange,
  onRuntimeBucketChange,
}: {
  className?: string;
  authorOptions: FacetOption[];
  narratorOptions: FacetOption[];
  seriesOptions: FacetOption[];
  genreOptions: FacetOption[];
  sourceOptions: FacetOption[];
  filterAuthors: string[];
  filterNarrators: string[];
  filterSeries: string[];
  filterGenres: string[];
  excludedSources: string[];
  /** Enabled storefront plugin ids from `/api/portal/sources`. */
  enabledSourceIds: string[];
  minRating: number | null;
  runtimeBucket: RuntimeBucket;
  onAuthorsChange: (next: string[]) => void;
  onNarratorsChange: (next: string[]) => void;
  onSeriesChange: (next: string[]) => void;
  onGenresChange: (next: string[]) => void;
  onExcludedSourcesChange: (next: string[]) => void;
  onMinRatingChange: (next: number | null) => void;
  onRuntimeBucketChange: (next: RuntimeBucket) => void;
}) {
  const stores = useMemo(() => {
    const enabled = enabledSourceIds.map((id) => id.toLowerCase());
    // Prefer enabled sources; intersect facet hits so disabled stores do not reappear.
    if (enabled.length === 0) return [];
    const fromFacets = new Set(
      sourceOptions.map((o) => o.value.toLowerCase()).filter(Boolean),
    );
    const ids = enabled.filter((id) => fromFacets.size === 0 || fromFacets.has(id));
    return (ids.length > 0 ? ids : enabled).sort();
  }, [enabledSourceIds, sourceOptions]);

  function toggleSource(id: string, included: boolean) {
    const key = id.toLowerCase();
    if (included) {
      onExcludedSourcesChange(
        excludedSources.filter((s) => s.toLowerCase() !== key),
      );
    } else if (!excludedSources.some((s) => s.toLowerCase() === key)) {
      onExcludedSourcesChange([...excludedSources, key]);
    }
  }

  return (
    <aside
      className={cn(
        "flex h-full min-h-0 w-full flex-col gap-4 overflow-y-auto overscroll-contain pr-1",
        className,
      )}
    >
      <FilterSection title="Stores">
        {stores.length === 0 ? (
          <p className="flex items-center gap-2 text-sm text-ink/55">
            <Ban className="h-4 w-4 shrink-0 text-ink/40" aria-hidden />
            <span>None</span>
          </p>
        ) : (
          <ul className="space-y-2">
            {stores.map((id) => {
              const included = !excludedSources.some(
                (s) => s.toLowerCase() === id,
              );
              return (
                <li key={id}>
                  <label className="flex cursor-pointer items-center gap-2 text-sm text-ink">
                    <input
                      type="checkbox"
                      checked={included}
                      onChange={(e) => toggleSource(id, e.target.checked)}
                    />
                    <StoreLogo source={id} className="h-4 w-4" />
                    <span>{storeLabel(id)}</span>
                  </label>
                </li>
              );
            })}
          </ul>
        )}
      </FilterSection>

      <FilterSection title="Customer reviews">
        <ul className="space-y-1.5 text-sm">
          {(
            [
              [null, "Any rating"],
              [4, "4★ & up"],
              [3, "3★ & up"],
            ] as const
          ).map(([value, label]) => (
            <li key={label}>
              <label className="flex cursor-pointer items-center gap-2 text-ink">
                <input
                  type="radio"
                  name="discover-min-rating"
                  checked={minRating === value}
                  onChange={() => onMinRatingChange(value)}
                />
                <span>{label}</span>
              </label>
            </li>
          ))}
        </ul>
      </FilterSection>

      <FilterSection title="Runtime">
        <ul className="space-y-1.5 text-sm">
          {RUNTIME_BUCKET_OPTIONS.map((opt) => (
            <li key={opt.value}>
              <label className="flex cursor-pointer items-center gap-2 text-ink">
                <input
                  type="radio"
                  name="discover-runtime"
                  checked={runtimeBucket === opt.value}
                  onChange={() => onRuntimeBucketChange(opt.value)}
                />
                <span>{opt.label}</span>
              </label>
            </li>
          ))}
        </ul>
      </FilterSection>

      <FilterSection title="Author">
        <SearchableCheckList
          options={authorOptions}
          selected={filterAuthors}
          onChange={onAuthorsChange}
          placeholder="Find an author"
          emptyLabel="Load more results for authors"
        />
      </FilterSection>

      <FilterSection title="Narrator">
        <SearchableCheckList
          options={narratorOptions}
          selected={filterNarrators}
          onChange={onNarratorsChange}
          placeholder="Find a narrator"
          emptyLabel="Load more results for narrators"
        />
      </FilterSection>

      <FilterSection title="Series">
        <SearchableCheckList
          options={seriesOptions}
          selected={filterSeries}
          onChange={onSeriesChange}
          placeholder="Find a series"
          emptyLabel="Load more results for series"
        />
      </FilterSection>

      <FilterSection title="Genre">
        <SearchableCheckList
          options={genreOptions}
          selected={filterGenres}
          onChange={onGenresChange}
          placeholder="Find a genre"
          emptyLabel="Load more results for genres"
        />
      </FilterSection>
    </aside>
  );
}
