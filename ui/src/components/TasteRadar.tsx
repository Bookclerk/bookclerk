import { useEffect, useId, useState } from "react";
import { fetchTasteRadar, type TasteRadarAxis, type TasteRadarQuery } from "@/lib/api";
import { cn } from "@/lib/utils";

/** How long the book's details must hold still before the radar is asked for. */
const SETTLE_MS = 300;

/** Width of the chart's drawing area, in SVG units: room for a label on each side. */
const WIDTH = 340;

/** Height of the chart's drawing area, in SVG units. */
const HEIGHT = 236;

/** Most characters of an axis's name a label shows. */
const LABEL_CHARS = 15;

/** Distance from the centre to a score of 1. */
const RADIUS = 78;

/** Distance from the centre to an axis label. */
const LABEL_RADIUS = 96;

/** Distance from the centre a wedge answers a pointer to: the chart's edge. */
const REACH = HEIGHT / 2 - 2;

/** The rings drawn behind the shape, as scores. */
const RINGS = [0.25, 0.5, 0.75, 1];

/** A point `distance` from the centre toward `turn` (0 is up, 1 a full turn). */
function point(turn: number, distance: number): [number, number] {
  const angle = turn * 2 * Math.PI - Math.PI / 2;
  return [WIDTH / 2 + distance * Math.cos(angle), HEIGHT / 2 + distance * Math.sin(angle)];
}

/** `points` as the `points` attribute of a polygon. */
function polygon(points: [number, number][]): string {
  return points.map(([x, y]) => `${x.toFixed(1)},${y.toFixed(1)}`).join(" ");
}

/**
 * Says a score in words.
 *
 * @param score - How close the book is to an axis, 0 to 1.
 * @returns The start of a sentence, such as "Very close to".
 */
function closeness(score: number): string {
  if (score >= 0.75) return "Very close to";
  if (score >= 0.5) return "Close to";
  if (score >= 0.25) return "Some overlap with";
  return "Far from";
}

/** "these 6 books and wishes of yours", for an axis made by `books` works. */
function yourBooks(books: number): string {
  return books === 1 ? "this book or wish of yours" : `these ${books} books and wishes of yours`;
}

/** One line a screen reader hears for an axis. */
function spoken(axis: TasteRadarAxis): string {
  const like =
    axis.examples.length > 0
      ? ` Nearest of yours: ${axis.examples.map((e) => e.title).join(", ")}.`
      : "";
  return `${axis.name}: ${closeness(axis.score).toLowerCase()} ${yourBooks(axis.books)}.${like}`;
}

/** Props of {@link TasteRadarChart}. */
export interface TasteRadarChartProps {
  /** Three to six axes, each with the book placed on it. */
  axes: TasteRadarAxis[];
}

/**
 * The radar itself: one wedge for each axis of a person's taste (their
 * books and wishes, grouped by meaning), the shape reaching further out
 * where the book lands closer to that group.
 * Pointing at a wedge, focusing it or tapping it shows that axis's tile:
 * how close the book is and which of the person's own books are nearest.
 * A tap keeps the tile until another wedge is tapped, so it also works
 * without a pointer. The tile sits beside the chart, or under it on a
 * phone.
 */
export function TasteRadarChart({ axes }: TasteRadarChartProps) {
  const [hovered, setHovered] = useState<number | null>(null);
  const [pinned, setPinned] = useState<number | null>(null);
  const tileId = useId();
  if (axes.length < 3) return null;
  const strongest = axes.reduce((best, axis, i) => (axis.score > axes[best]!.score ? i : best), 0);
  const shownIndex = hovered ?? pinned ?? strongest;
  const shown = axes[Math.min(shownIndex, axes.length - 1)]!;
  const step = 1 / axes.length;
  const shape = axes.map((axis, i) =>
    // A sliver even at 0, so the shape never collapses to a line.
    point(i * step, RADIUS * Math.max(axis.score, 0.06)),
  );
  return (
    <div className="flex flex-col items-center gap-3 sm:flex-row sm:items-start">
      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        className="h-auto w-full max-w-80 shrink-0"
        role="group"
        aria-label="How close this book is to each part of your taste"
      >
        {RINGS.map((ring) => (
          <polygon
            key={ring}
            points={polygon(axes.map((_, i) => point(i * step, RADIUS * ring)))}
            className="fill-none stroke-ink/15"
            strokeWidth={1}
          />
        ))}
        {axes.map((axis, i) => {
          const [x, y] = point(i * step, RADIUS);
          return (
            <line
              key={axis.name}
              x1={WIDTH / 2}
              y1={HEIGHT / 2}
              x2={x}
              y2={y}
              className="stroke-ink/15"
              strokeWidth={1}
            />
          );
        })}
        <polygon
          points={polygon(shape)}
          className="fill-teal/25 stroke-teal"
          strokeWidth={2}
          strokeLinejoin="round"
        />
        {axes.map((axis, i) => {
          const active = i === shownIndex;
          const [dotX, dotY] = shape[i]!;
          const [labelX, labelY] = point(i * step, LABEL_RADIUS);
          const anchor =
            Math.abs(labelX - WIDTH / 2) < 1 ? "middle" : labelX > WIDTH / 2 ? "start" : "end";
          // The wedge reaches half a step to each side of its axis and out to
          // the chart's edge, so the whole slice answers a pointer.
          const [left, tip, right] = [i - 1, i, i + 1].map((at) => point(at * step, RADIUS));
          const between = (a: [number, number], b: [number, number]): [number, number] => [
            (a[0] + b[0]) / 2,
            (a[1] + b[1]) / 2,
          ];
          // What lights up: the axis's share of the outer ring.
          const lit = polygon([
            [WIDTH / 2, HEIGHT / 2],
            between(left!, tip!),
            tip!,
            between(tip!, right!),
          ]);
          const wedge = polygon([
            [WIDTH / 2, HEIGHT / 2],
            point((i - 0.5) * step, REACH),
            point(i * step, REACH),
            point((i + 0.5) * step, REACH),
          ]);
          return (
            <g
              key={axis.name}
              role="button"
              tabIndex={0}
              aria-label={spoken(axis)}
              aria-pressed={pinned === i}
              aria-describedby={tileId}
              className="cursor-pointer focus:outline-none [&:focus-visible>.lit]:stroke-teal"
              onPointerEnter={(e) => {
                if (e.pointerType === "mouse") setHovered(i);
              }}
              onPointerLeave={() => setHovered(null)}
              onFocus={() => setHovered(i)}
              onBlur={() => setHovered(null)}
              onClick={() => setPinned(i)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  setPinned(i);
                }
              }}
            >
              <polygon points={wedge} className="fill-transparent" />
              <polygon
                points={lit}
                className={cn("lit", active ? "fill-teal/10" : "fill-transparent")}
                strokeWidth={1.5}
              />
              <circle
                cx={dotX}
                cy={dotY}
                r={active ? 4.5 : 3}
                className="fill-teal stroke-paper"
                strokeWidth={1.5}
              />
              <text
                x={labelX}
                y={labelY}
                textAnchor={anchor}
                dominantBaseline="middle"
                className={cn(
                  "text-[11px]",
                  active ? "fill-ink font-semibold" : "fill-ink/70 font-medium",
                )}
              >
                {axis.name.length > LABEL_CHARS
                  ? `${axis.name.slice(0, LABEL_CHARS - 1)}…`
                  : axis.name}
              </text>
            </g>
          );
        })}
      </svg>
      <div
        id={tileId}
        aria-live="polite"
        className="w-full min-w-0 rounded-md bg-card-mid px-3 py-2 ring-1 ring-ink/5 sm:mt-6 sm:flex-1"
      >
        <p className="text-sm font-semibold text-ink">{shown.name}</p>
        <p className="text-sm text-teal-text">
          {closeness(shown.score)}
          <span className="text-ink/70"> {yourBooks(shown.books)}</span>
        </p>
        {shown.examples.length > 0 ? (
          <>
            <p className="mt-2 text-xs font-semibold uppercase tracking-wide text-ink/70">
              Nearest of yours
            </p>
            <ul className="mt-0.5 space-y-0.5">
              {shown.examples.map((example) => (
                <li key={example.work_id} className="truncate text-sm text-ink">
                  {example.title}
                </li>
              ))}
            </ul>
          </>
        ) : null}
      </div>
    </div>
  );
}

/** Props of {@link TasteRadar}. */
export interface TasteRadarProps {
  /** The library work shown, when the dialog shows one. */
  workId?: string | null;
  /** Display title; a store title is read from this and the fields below. */
  title: string;
  /** Authors, as one comma-separated line. */
  authors?: string | null;
  /** Series names, as one comma-separated line. */
  series?: string | null;
  /** Genres or categories, as one line. */
  genres?: string | null;
  /** Synopsis. */
  description?: string | null;
}

/**
 * "How it fits your taste": the radar of the book a dialog shows. Asks for
 * it once the book's details have held still for a moment, since a dialog
 * fills them in as stores answer. Renders nothing while it loads, when the
 * library has no embedding model, or when the viewer's taste has too few
 * books and wishes that sit together to draw from.
 */
export function TasteRadar({
  workId,
  title,
  authors,
  series,
  genres,
  description,
}: TasteRadarProps) {
  // What to ask about: the work alone when there is one.
  const query: TasteRadarQuery = workId
    ? { work_id: workId }
    : { title, authors, series, genres, description };
  const key = JSON.stringify(query);
  // The book, whatever is known of it so far.
  const book = workId ?? title;
  // The axes read, with the book they place.
  const [read, setRead] = useState<{ book: string; axes: TasteRadarAxis[] } | null>(null);

  useEffect(() => {
    let current = true;
    const timer = window.setTimeout(() => {
      fetchTasteRadar(JSON.parse(key) as TasteRadarQuery)
        .then((res) => {
          if (current) setRead({ book, axes: res.state === "ready" ? res.axes : [] });
        })
        .catch(() => {
          // An extra of the dialog: without it the dialog is complete.
          if (current) setRead({ book, axes: [] });
        });
    }, SETTLE_MS);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [key, book]);

  // While more details of the same book are on their way, the radar
  // already drawn stays.
  const axes = read?.book === book ? read.axes : [];
  if (axes.length < 3) return null;
  return (
    <section className="space-y-2 border-t border-ink/10 pt-3" aria-label="How it fits your taste">
      <h3 className="font-display text-sm font-semibold text-ink">How it fits your taste</h3>
      <TasteRadarChart axes={axes} />
    </section>
  );
}
