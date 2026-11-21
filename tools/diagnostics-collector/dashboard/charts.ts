/**
 * Server-drawn SVG charts. Everything is computed from numbers and fixed
 * class names; labels pass through `html`, so report text cannot inject
 * markup. Colors come from CSS classes in public/static/assets/dashboard.*.css (the
 * page CSP allows no inline styles), so both themes apply.
 */

import { html, raw, type SafeHtml } from "./html.ts";

/**
 * A round axis maximum and step for a data maximum: steps of 1, 2 or 5 times
 * a power of ten, at most five steps.
 *
 * @param max - Largest value (>= 0).
 * @returns Axis top and tick step.
 */
export function niceScale(max: number): { top: number; step: number } {
  if (!(max > 0)) return { top: 4, step: 1 };
  const rough = max / 4;
  const pow = 10 ** Math.floor(Math.log10(rough));
  const step = [1, 2, 5, 10].map((m) => m * pow).find((s) => s >= rough) ?? 10 * pow;
  const whole = Math.max(1, Math.round(step));
  return { top: Math.ceil(max / whole) * whole, step: whole };
}

/**
 * Rounds to two decimals for compact SVG.
 *
 * @param n - Coordinate.
 * @returns Rounded coordinate.
 */
function r2(n: number): number {
  return Math.round(n * 100) / 100;
}

/** A chart's drawing size, in SVG units, and how many x labels fit. */
interface Frame {
  /** Width. */
  w: number;
  /** Height. */
  h: number;
  /** At most this many x-axis labels. */
  labels: number;
}

/**
 * One size the bar charts are drawn at. Each chart is drawn twice, wide and
 * narrow, and the stylesheet shows the one that fits (`.chart.wide` above
 * 620 px, `.chart.narrow` below), so the axis text stays legible on a phone
 * instead of shrinking with a wide drawing.
 */
interface ChartSize {
  /** The class that shows this drawing. */
  cls: "wide" | "narrow";
  /** The stacked bar chart's frame. */
  bars: Frame;
  /** The histogram's frame. */
  histogram: Frame;
}

/** Desktop and tablet drawings. */
const WIDE: ChartSize = {
  cls: "wide",
  bars: { w: 760, h: 230, labels: 8 },
  histogram: { w: 520, h: 120, labels: 5 },
};

/** Phone drawings, about as wide as a phone panel, so one unit is one pixel. */
const NARROW: ChartSize = {
  cls: "narrow",
  bars: { w: 340, h: 200, labels: 4 },
  histogram: { w: 340, h: 120, labels: 4 },
};

/**
 * Stacked bar chart of counts per bucket.
 *
 * @param buckets - One per
 *   x position, oldest first.
 * @param series - Stack order, bottom first;
 *   `cls` is the CSS class that colors it.
 * @param ariaLabel - Accessible description.
 * @returns An `<svg>`.
 */
export function stackedBars(
  buckets: { label: string; values: Record<string, number> }[],
  series: { key: string; cls: string }[],
  ariaLabel: string,
): SafeHtml {
  return html`${stackedBarsAt(buckets, series, ariaLabel, WIDE)}${stackedBarsAt(buckets, series, ariaLabel, NARROW)}`;
}

/**
 * One drawing of {@link stackedBars} at a given size.
 *
 * @param buckets - As for stackedBars.
 * @param series - As for stackedBars.
 * @param ariaLabel - As for stackedBars.
 * @param size - Drawing size and the class that shows it.
 * @returns An `<svg>`.
 */
function stackedBarsAt(
  buckets: { label: string; values: Record<string, number> }[],
  series: { key: string; cls: string }[],
  ariaLabel: string,
  size: ChartSize,
): SafeHtml {
  const W = size.bars.w;
  const H = size.bars.h;
  const L = 40;
  const R = 8;
  const T = 16;
  const B = 26;
  const totals = buckets.map((b) => series.reduce((s, x) => s + (b.values[x.key] || 0), 0));
  const { top, step } = niceScale(Math.max(0, ...totals));
  const y = (v: number): number => T + (H - T - B) * (1 - v / top);
  const bw = (W - L - R) / Math.max(1, buckets.length);
  const gap = Math.min(4, bw * 0.15);
  const parts: string[] = [];
  for (let t = 0; t <= top; t += step) {
    parts.push(
      `<line class="grid" x1="${L}" x2="${W - R}" y1="${r2(y(t))}" y2="${r2(y(t))}"/>`,
      `<text class="tick" x="${L - 6}" y="${r2(y(t) + 4)}" text-anchor="end">${t}</text>`,
    );
  }
  const labelEvery = Math.ceil(buckets.length / size.bars.labels);
  buckets.forEach((b, i) => {
    let acc = 0;
    const x = L + i * bw + gap;
    for (const s of series) {
      const v = b.values[s.key] || 0;
      if (!v) continue;
      parts.push(
        `<rect class="${s.cls}" x="${r2(x)}" y="${r2(y(acc + v))}" width="${r2(bw - 2 * gap)}" height="${r2(y(acc) - y(acc + v))}"/>`,
      );
      acc += v;
    }
    const last = i === buckets.length - 1;
    if ((buckets.length - 1 - i) % labelEvery === 0) {
      parts.push(
        `<text class="tick" x="${r2(L + i * bw + bw / 2)}" y="${H - 8}" text-anchor="middle">${html`${b.label}`}</text>`,
      );
    }
    const total = totals[i] ?? 0;
    if (last && total > 0) {
      parts.push(
        `<text class="tick strong" x="${r2(L + i * bw + bw / 2)}" y="${r2(y(total) - 4)}" text-anchor="middle">${total}</text>`,
      );
    }
  });
  return html`<svg class="chart ${size.cls}" viewBox="0 0 ${W} ${H}" role="img" aria-label="${ariaLabel}">${raw(parts.join(""))}</svg>`;
}

/**
 * Sparkline with an area fill and an emphasized last point.
 *
 * @param values - Oldest first.
 * @param cls - CSS class for stroke and fill color.
 * @param w - Width in px.
 * @param h - Height in px.
 * @param stretch - Fill the container's width instead of keeping
 *   the aspect ratio.
 * @returns An `<svg>`.
 */
export function sparkline(
  values: number[],
  cls: string,
  w: number = 90,
  h: number = 22,
  stretch: boolean = false,
): SafeHtml {
  if (values.length < 2) values = [values[0] ?? 0, values[0] ?? 0];
  const max = Math.max(1, ...values);
  const pts = values.map((v, i): [number, number] => [
    r2(2 + (i * (w - 4)) / (values.length - 1)),
    r2(h - 3 - ((h - 6) * v) / max),
  ]);
  const line = pts.map((p) => p.join(",")).join(" L");
  // At least two points: shorter inputs were padded above.
  const [lx, ly] = pts[pts.length - 1] ?? [0, 0];
  const x0 = pts[0]?.[0] ?? 0;
  return raw(
    `<svg class="spark ${cls}" viewBox="0 0 ${w} ${h}" width="${w}" height="${h}"${stretch ? ' preserveAspectRatio="none"' : ""} aria-hidden="true">` +
      `<path class="area" d="M${line} L${lx},${h} L${x0},${h} Z"/>` +
      `<path class="line" d="M${line}"/><circle class="dot" cx="${lx}" cy="${ly}" r="2.4"/></svg>`,
  );
}

/**
 * Horizontal meter bar.
 *
 * @param fraction - 0–1.
 * @param cls - CSS class for the filled part.
 * @param ariaLabel - Accessible description; omitted makes it
 *   decorative.
 * @returns An `<svg>`.
 */
export function meter(
  fraction: number,
  cls: string = "fill-teal",
  ariaLabel: string = "",
): SafeHtml {
  const f = Math.max(0, Math.min(1, Number(fraction) || 0));
  const a11y = ariaLabel ? html`role="img" aria-label="${ariaLabel}"` : raw('aria-hidden="true"');
  return html`<svg class="meter" viewBox="0 0 100 8" preserveAspectRatio="none" ${a11y}><rect class="track" x="0" y="0" width="100" height="8" rx="4"/>${raw(
    f > 0 ? `<rect class="${cls}" x="0" y="0" width="${r2(f * 100)}" height="8" rx="4"/>` : "",
  )}</svg>`;
}

/**
 * Daily bar histogram with an optional dashed marker (a fix merging).
 *
 * @param days - Oldest first;
 *   `after` bars draw in the alert color.
 * @param marker - Bar index and label.
 * @param ariaLabel - Accessible description.
 * @returns An `<svg>`.
 */
export function histogram(
  days: { label: string; n: number; after: boolean }[],
  marker: { index: number; label: string } | null,
  ariaLabel: string,
): SafeHtml {
  return html`${histogramAt(days, marker, ariaLabel, WIDE)}${histogramAt(days, marker, ariaLabel, NARROW)}`;
}

/**
 * One drawing of {@link histogram} at a given size.
 *
 * @param days - As for histogram.
 * @param marker - As for histogram.
 * @param ariaLabel - As for histogram.
 * @param size - Drawing size and the class that shows it.
 * @returns An `<svg>`.
 */
function histogramAt(
  days: { label: string; n: number; after: boolean }[],
  marker: { index: number; label: string } | null,
  ariaLabel: string,
  size: ChartSize,
): SafeHtml {
  const W = size.histogram.w;
  const H = size.histogram.h;
  const L = 28;
  const R = 6;
  const T = 14;
  const B = 22;
  const { top, step } = niceScale(Math.max(0, ...days.map((d) => d.n)));
  const y = (v: number): number => T + (H - T - B) * (1 - v / top);
  const bw = (W - L - R) / Math.max(1, days.length);
  const parts: string[] = [];
  for (let t = 0; t <= top; t += step) {
    parts.push(
      `<line class="grid" x1="${L}" x2="${W - R}" y1="${r2(y(t))}" y2="${r2(y(t))}"/>`,
      `<text class="tick" x="${L - 5}" y="${r2(y(t) + 4)}" text-anchor="end">${t}</text>`,
    );
  }
  const labelEvery = Math.ceil(days.length / size.histogram.labels);
  days.forEach((d, i) => {
    if (d.n > 0) {
      parts.push(
        `<rect class="${d.after ? "fill-brick" : "fill-teal"}" x="${r2(L + i * bw + 3)}" y="${r2(y(d.n))}" width="${r2(Math.max(1, bw - 6))}" height="${r2(y(0) - y(d.n))}"/>`,
      );
    }
    if ((days.length - 1 - i) % labelEvery === 0) {
      parts.push(
        `<text class="tick" x="${r2(L + i * bw + bw / 2)}" y="${H - 6}" text-anchor="middle">${html`${d.label}`}</text>`,
      );
    }
  });
  if (marker && marker.index >= 0 && marker.index < days.length) {
    const fx = r2(L + marker.index * bw + bw / 2);
    const anchor = marker.index > days.length * 0.7 ? "end" : "start";
    parts.push(
      `<line class="marker" x1="${fx}" x2="${fx}" y1="${T - 4}" y2="${r2(y(0))}"/>`,
      `<text class="tick strong" x="${anchor === "end" ? fx - 4 : fx + 4}" y="${T + 4}" text-anchor="${anchor}">${html`${marker.label}`}</text>`,
    );
  }
  return html`<svg class="chart ${size.cls}" viewBox="0 0 ${W} ${H}" role="img" aria-label="${ariaLabel}">${raw(parts.join(""))}</svg>`;
}
