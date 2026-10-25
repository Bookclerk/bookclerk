/**
 * Server-drawn SVG charts. Everything is computed from numbers and fixed
 * class names; labels pass through `html`, so report text cannot inject
 * markup. Colors come from CSS classes in public/static/dashboard.css (the
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
export function niceScale(max: number): { top: number; step: number; } {
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
export function stackedBars(buckets: { label: string; values: Record<string, number>; }[], series: { key: string; cls: string; }[], ariaLabel: string): SafeHtml {
  const W = 760;
  const H = 230;
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
  const labelEvery = Math.ceil(buckets.length / 8);
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
      parts.push(`<text class="tick" x="${r2(L + i * bw + bw / 2)}" y="${H - 8}" text-anchor="middle">${html`${b.label}`}</text>`);
    }
    const total = totals[i] ?? 0;
    if (last && total > 0) {
      parts.push(`<text class="tick strong" x="${r2(L + i * bw + bw / 2)}" y="${r2(y(total) - 4)}" text-anchor="middle">${total}</text>`);
    }
  });
  return html`<svg class="chart" viewBox="0 0 ${W} ${H}" role="img" aria-label="${ariaLabel}">${raw(parts.join(""))}</svg>`;
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
export function sparkline(values: number[], cls: string, w: number = 90, h: number = 22, stretch: boolean = false): SafeHtml {
  if (values.length < 2) values = [values[0] ?? 0, values[0] ?? 0];
  const max = Math.max(1, ...values);
  const pts = values.map((v, i): [number, number] => [r2(2 + (i * (w - 4)) / (values.length - 1)), r2(h - 3 - ((h - 6) * v) / max)]);
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
export function meter(fraction: number, cls: string = "fill-teal", ariaLabel: string = ""): SafeHtml {
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
export function histogram(days: { label: string; n: number; after: boolean; }[], marker: { index: number; label: string; } | null, ariaLabel: string): SafeHtml {
  const W = 520;
  const H = 120;
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
  const labelEvery = Math.ceil(days.length / 5);
  days.forEach((d, i) => {
    if (d.n > 0) {
      parts.push(
        `<rect class="${d.after ? "fill-brick" : "fill-teal"}" x="${r2(L + i * bw + 3)}" y="${r2(y(d.n))}" width="${r2(Math.max(1, bw - 6))}" height="${r2(y(0) - y(d.n))}"/>`,
      );
    }
    if ((days.length - 1 - i) % labelEvery === 0) {
      parts.push(`<text class="tick" x="${r2(L + i * bw + bw / 2)}" y="${H - 6}" text-anchor="middle">${html`${d.label}`}</text>`);
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
  return html`<svg class="chart" viewBox="0 0 ${W} ${H}" role="img" aria-label="${ariaLabel}">${raw(parts.join(""))}</svg>`;
}
