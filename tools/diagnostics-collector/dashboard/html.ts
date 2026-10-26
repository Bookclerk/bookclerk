/**
 * HTML building and page responses for the dashboard.
 *
 * Every page is built with the `html` tagged template, which escapes each
 * interpolated value unless it is already `SafeHtml`. Report and issue text is
 * hostile (anyone can submit a report), so nothing reaches a page without
 * passing through `escapeHtml`, and nothing from a report is placed in an
 * attribute other than through the same escape.
 */

/** HTML that has already been escaped or built from trusted markup. */
export class SafeHtml {
  /** The markup. */
  readonly value: string;

  /**
   * @param value - Markup to emit as is.
   */
  constructor(value: string) {
    this.value = value;
  }

  /** @returns The markup. */
  toString(): string {
    return this.value;
  }
}

/**
 * Escapes text for an HTML text node or a double-quoted attribute.
 *
 * @param value - Any value; null and undefined become empty.
 * @returns Escaped text.
 */
export function escapeHtml(value: unknown): string {
  if (value === null || value === undefined) return "";
  return String(value).replace(/[&<>"']/g, (c) => ESCAPES[c as keyof typeof ESCAPES]);
}

/** Replacement for each character that is special in HTML. */
const ESCAPES: Readonly<Record<"&" | "<" | ">" | '"' | "'", string>> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

/**
 * Renders one interpolated value: `SafeHtml` as is, arrays joined, anything
 * else escaped.
 *
 * @param value - Interpolated value.
 * @returns Markup.
 */
function part(value: unknown): string {
  if (value instanceof SafeHtml) return value.value;
  if (Array.isArray(value)) return value.map(part).join("");
  if (value === false || value === null || value === undefined) return "";
  return escapeHtml(value);
}

/**
 * Tagged template that escapes every interpolation.
 *
 * @param strings - Literal parts.
 * @param values - Interpolated values.
 * @returns The joined markup.
 */
export function html(strings: TemplateStringsArray, ...values: unknown[]): SafeHtml {
  let out = strings[0] ?? "";
  for (let i = 0; i < values.length; i++) {
    out += part(values[i]) + (strings[i + 1] ?? "");
  }
  return new SafeHtml(out);
}

/**
 * Marks trusted markup (built by this module's callers from constants and
 * numbers only) as safe.
 *
 * @param markup - Markup with no report-derived text in it.
 * @returns The same markup.
 */
export function raw(markup: string): SafeHtml {
  return new SafeHtml(markup);
}

/**
 * Content-Security-Policy for every dashboard page: only this origin's
 * styles, scripts, fonts and images; forms post to this origin; no framing.
 */
export const DASHBOARD_CSP =
  "default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

/** Headers every dashboard response carries. */
export const SECURITY_HEADERS = {
  "Content-Security-Policy": DASHBOARD_CSP,
  "Cache-Control": "no-store",
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Cross-Origin-Opener-Policy": "same-origin",
  "Permissions-Policy": "camera=(), microphone=(), geolocation=(), interest-cohort=()",
  "Strict-Transport-Security": "max-age=31536000",
};

/**
 * An HTML response with the dashboard's security headers.
 *
 * @param body - The whole document.
 * @param status - HTTP status.
 * @param extra - Additional headers (for example
 *   `Set-Cookie`, which may repeat via `cookies`).
 * @param cookies - `Set-Cookie` values.
 * @returns The response.
 */
export function htmlResponse(body: SafeHtml, status: number = 200, extra: Record<string, string> = {}, cookies: string[] = []): Response {
  const headers = new Headers({
    "Content-Type": "text/html; charset=utf-8",
    ...SECURITY_HEADERS,
    ...extra,
  });
  for (const c of cookies) headers.append("Set-Cookie", c);
  return new Response(String(body), { status, headers });
}

/**
 * A redirect with the dashboard's security headers.
 *
 * @param location - Same-origin path or an absolute GitHub URL.
 * @param cookies - `Set-Cookie` values.
 * @returns A 303 response.
 */
export function redirect(location: string, cookies: string[] = []): Response {
  const headers = new Headers({ Location: location, ...SECURITY_HEADERS });
  for (const c of cookies) headers.append("Set-Cookie", c);
  return new Response(null, { status: 303, headers });
}

/**
 * Formats a count with thousands separators.
 *
 * @param n - Count.
 * @returns For example `1,240`.
 */
export function fmtInt(n: number): string {
  return Math.round(Number(n) || 0).toLocaleString("en-US");
}

/**
 * ISO-8601 text for a Unix time, safe on untrusted numbers.
 *
 * @param ms - Unix time in milliseconds.
 * @returns `YYYY-MM-DDTHH:MM:SS.sssZ`, or an empty string when the
 *   value is not a number inside the Date range (±8.64e15 ms).
 */
export function isoTime(ms: unknown): string {
  if (typeof ms !== "number" || !Number.isFinite(ms) || Math.abs(ms) > 8.64e15) return "";
  return new Date(ms).toISOString();
}

/**
 * Formats a Unix time as UTC `YYYY-MM-DD HH:MM`.
 *
 * @param ms - Unix time in milliseconds.
 * @returns The time, or an empty string when not a valid time.
 */
export function fmtTime(ms: number): string {
  return isoTime(ms).slice(0, 16).replace("T", " ");
}

/**
 * Formats a Unix time as a short UTC date, `Oct 23`.
 *
 * @param ms - Unix time in milliseconds.
 * @returns The date.
 */
export function fmtDay(ms: number): string {
  if (!Number.isFinite(ms)) return "";
  const d = new Date(ms);
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCDate()}`;
}

/** Short month names for `fmtDay`. */
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
