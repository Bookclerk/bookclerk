// The demo Worker's own pages: the launch page with the Turnstile check, the
// waiting page, and the pages for a busy demo or an ended session. Styles,
// scripts, fonts and the mark are static files under public/_demo/, so the
// pages' CSP needs no inline code. Nothing loads from another origin except
// Turnstile.

/** Content-Security-Policy for the Worker's pages. Turnstile is the only outside origin. */
export const PAGE_CSP = [
  "default-src 'none'",
  "script-src 'self' https://challenges.cloudflare.com",
  "frame-src https://challenges.cloudflare.com",
  "style-src 'self'",
  "img-src 'self' data:",
  "font-src 'self'",
  "connect-src 'self'",
  "form-action 'self'",
  "frame-ancestors 'none'",
  "base-uri 'none'",
].join("; ");

/** Turnstile's script, rendering on the callback public/_demo/launch.js defines. */
export const TURNSTILE_SCRIPT =
  "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit&onload=bookclerkDemoTurnstile";

/** Headers every Worker page and proxied response carries. */
export const SECURITY_HEADERS = Object.freeze({
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Referrer-Policy": "same-origin",
  "Strict-Transport-Security": "max-age=31536000",
});

/** The footer note the website carries too (tools/site/src/partials/footer.html), word for word. */
export const LEGAL =
  "Bookclerk is free software under the GNU AGPL, version 3 or later. It is not affiliated with or endorsed by Audible, Libro.fm, Chirp or GraphicAudio; their names are trademarks of their owners. Bookclerk is meant for books you have bought, for your own listening.";

/**
 * Escapes text for HTML element content and quoted attributes.
 *
 * @param s - Text.
 * @returns Escaped text.
 */
export function esc(s: string): string {
  return String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

/**
 * The host name of the website, for "Back to bookclerk.app".
 *
 * @param site - The website's https URL.
 * @returns Its host, or "Bookclerk" when the URL does not parse.
 */
function hostOf(site: string): string {
  try {
    return new URL(site).host || "Bookclerk";
  } catch {
    return "Bookclerk";
  }
}

/**
 * Wraps page content in the shared document: the header with the mark and a
 * link back to the website, the content, and the footer note with a link to
 * the website's privacy page.
 *
 * @param title - Page title.
 * @param description - One or two plain-text sentences on what the page
 *   is for, for the description tag search engines show and check for (the
 *   pages are `noindex`, but crawlers still read them).
 * @param body - Body HTML (already escaped).
 * @param opts - Script URLs to load, deferred; the website to link back to
 *   (an https URL, or empty for none); and `card` to set the content in a
 *   centred card (the waiting and ended pages) rather than across the page
 *   (the launch page).
 * @returns The document.
 */
function documentHtml(title: string, description: string, body: string, opts: { scripts?: string[]; siteUrl?: string; card?: boolean; } = {}): string {
  const scripts = (opts.scripts ?? []).map((src) => `<script src="${esc(src)}" defer></script>`).join("");
  const site = opts.siteUrl ?? "";
  const home = site ? esc(site) : "/";
  const back = site ? `<a class="demo-back" href="${esc(site)}">Back to ${esc(hostOf(site))}</a>` : "";
  const privacy = site ? `<p><a href="${esc(site.replace(/\/+$/, ""))}/privacy">Privacy</a></p>` : "";
  const main = opts.card ? `<main class="demo-card">\n${body}\n</main>` : `<main class="demo-launch">\n${body}\n</main>`;
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>${esc(title)}</title>
<meta name="description" content="${esc(description)}">
<link rel="preload" href="/_demo/fonts/literata-v40-latin.woff2" as="font" type="font/woff2" crossorigin>
<link rel="preload" href="/_demo/fonts/source-sans-3-v20-latin.woff2" as="font" type="font/woff2" crossorigin>
<link rel="stylesheet" href="/_demo/demo.css">
${scripts}
</head>
<body>
<div class="demo-page">
<header class="demo-header">
<a class="demo-brand" href="${home}"><img src="/_demo/mark.svg" alt="" width="36" height="36"><span>Bookclerk</span></a>
${back}
</header>
${main}
<footer class="demo-footer"><p>${esc(LEGAL)}</p>${privacy}</footer>
</div>
</body>
</html>`;
}

/**
 * An HTML response with the page CSP and security headers.
 *
 * @param html - Document.
 * @param status - HTTP status.
 * @param extra - More headers.
 * @returns The response.
 */
export function htmlResponse(html: string, status: number = 200, extra: Record<string, string> = {}): Response {
  return new Response(html, {
    status,
    headers: {
      "Content-Type": "text/html; charset=utf-8",
      "Cache-Control": "no-store",
      "Content-Security-Policy": PAGE_CSP,
      ...SECURITY_HEADERS,
      ...extra,
    },
  });
}

/**
 * How long a start usually takes, in words.
 *
 * @param seconds - Typical start in whole seconds.
 * @returns "about 20 seconds", "about a minute" or "about 2 minutes".
 */
export function aboutHowLong(seconds: number): string {
  if (seconds < 60) return `about ${seconds} seconds`;
  const minutes = Math.max(1, Math.round(seconds / 60));
  return minutes === 1 ? "about a minute" : `about ${minutes} minutes`;
}

/**
 * An outline icon from the app's icon set (Lucide), coloured by demo.css.
 *
 * @param paths - The icon's SVG elements.
 * @returns The inline SVG, hidden from screen readers.
 */
function icon(paths: string): string {
  return `<svg class="demo-icon" viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false">${paths}</svg>`;
}

/** Lucide `lock`: the private copy. */
const LOCK = icon('<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>');

/** Lucide `key-round`: sign-ins. */
const KEY = icon(
  '<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5" fill="currentColor"/>',
);

/** Lucide `download`, struck through: nothing is downloaded. */
const NO_DOWNLOAD = icon(
  '<path d="M12 15V3"/><path d="m7 10 5 5 5-5"/><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="M3 3l18 18"/>',
);

/** Lucide `log-out`: signed out at the end. */
const LOG_OUT = icon('<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5"/><path d="M21 12H9"/>');

/** The stores a visitor can sign in to, in the website's order. */
const STORES = ["Audible", "Libro.fm", "Chirp", "GraphicAudio"];

/**
 * What the visitor is told before they start.
 *
 * @param sessionMinutes - Hard length of a session.
 * @returns The list.
 */
function notes(sessionMinutes: number): string {
  const row = (svg: string, title: string, text: string): string =>
    `<li>${svg}<div><p class="demo-note-title">${title}</p><p>${text}</p></div></li>`;
  return `<ul class="demo-notes">
${row(LOCK, "Your own private copy", `Nobody else can see it. It is deleted when you leave, after 10 minutes without activity, or after ${sessionMinutes} minutes.`)}
${row(KEY, "Sign-ins stay between you and the store", "Your sign-in goes only to your private copy and to the store you choose.")}
${row(NO_DOWNLOAD, "Nothing is downloaded", "Acquiring and downloading books are turned off in the demo.")}
${row(LOG_OUT, "Signed out when it ends", "Bookclerk signs your store accounts out before your copy is deleted.")}
</ul>`;
}

/**
 * The launch page.
 *
 * @param p - Turnstile site key (null when the demo is not set up), an
 *   optional message to show above the button, the website to link back
 *   to, the hard length of a session in minutes, and how long a start
 *   usually takes in seconds (null while not known).
 * @returns The document.
 */
export function launchPage(p: {
  siteKey: string | null;
  message?: string;
  siteUrl: string;
  sessionMinutes: number;
  estimateSeconds: number | null;
}): string {
  const message = p.message ? `<p class="demo-message" role="alert">${esc(p.message)}</p>` : "";
  const estimate =
    p.estimateSeconds === null ? "" : `<p class="demo-estimate">Usually ready in ${aboutHowLong(p.estimateSeconds)}</p>`;
  const form = p.siteKey
    ? `<form method="post" action="/_demo/start" class="demo-start">
${message}
<div class="cf-turnstile" data-sitekey="${esc(p.siteKey)}" data-action="demo_start"></div>
<div class="demo-start-row">
<button type="submit">Start the demo</button>
${estimate}
</div>
<p class="demo-small">Up to ${p.sessionMinutes} minutes. No account or email needed.</p>
</form>`
    : `<div class="demo-start">${message}<p class="demo-message">The demo is not available right now.</p></div>`;
  const stores = STORES.map((name) => `<li>${esc(name)}</li>`).join("");
  return documentHtml(
    "Try Bookclerk",
    "Try Bookclerk, the self-hosted audiobook library manager, in a private copy that is deleted afterwards: sign in to a store you use and see your books.",
    `<section class="demo-intro">
<p class="demo-eyebrow">Live demo</p>
<h1>Try Bookclerk with your own stores</h1>
<p class="demo-lede">Bookclerk keeps the audiobooks you buy from different stores in one library. Start a private copy, sign in to a store you use, and see your books side by side.</p>
<ul class="demo-stores" aria-label="Stores you can sign in to">${stores}</ul>
${form}
</section>
<aside class="demo-before" aria-labelledby="demo-before-title">
<h2 id="demo-before-title">Before you start</h2>
${notes(p.sessionMinutes)}
</aside>`,
    {
      // launch.js first: it defines the callback api.js calls to render the
      // check at a size that fits the screen.
      scripts: p.siteKey ? ["/_demo/launch.js", TURNSTILE_SCRIPT] : [],
      siteUrl: p.siteUrl,
    },
  );
}

/**
 * The app's waveform loading indicator (ui/src/components/WaveformThrobber.tsx):
 * the logo's three bars pulsing from their centers. Animated by demo.css, so
 * the page's CSP needs no inline styles; wait.js stops it when starting fails.
 */
const WAVEFORM_THROBBER = `<svg id="demo-wait-throbber" class="demo-throbber" viewBox="0 0 79 114" aria-hidden="true" focusable="false">
<rect class="waveform-bar" x="0" y="23" width="17" height="68" rx="8.5"/>
<rect class="waveform-bar" x="31" y="0" width="17" height="114" rx="8.5"/>
<rect class="waveform-bar" x="62" y="26" width="17" height="62" rx="8.5"/>
</svg>`;

/**
 * The page shown while the visitor's copy starts. /_demo/live.js follows
 * the session and /_demo/wait.js moves on when it is ready.
 *
 * @param p - How long a start usually takes in seconds (null while not
 *   known), and the website to link back to.
 * @returns The document.
 */
export function waitPage(p: { estimateSeconds: number | null; siteUrl: string; }): string {
  const typical =
    p.estimateSeconds === null ? "This usually takes under a minute." : `This usually takes ${aboutHowLong(p.estimateSeconds)}.`;
  return documentHtml(
    "Starting your demo",
    "Your private copy of Bookclerk is starting.",
    `${WAVEFORM_THROBBER}
<h1>Starting your copy of Bookclerk</h1>
<p id="demo-wait-status" role="status">${typical}</p>
<noscript><p>This page needs JavaScript to notice when your copy is ready. <a href="/_demo/enter">Continue</a></p></noscript>`,
    { scripts: ["/_demo/live.js", "/_demo/wait.js"], siteUrl: p.siteUrl, card: true },
  );
}

/**
 * The page for a session that has ended (or never existed here).
 *
 * @param p - The website to link back to.
 * @returns The document.
 */
export function endedPage(p: { siteUrl: string; }): string {
  return documentHtml(
    "Demo ended",
    "Your Bookclerk demo has ended and your copy has been deleted.",
    `<h1>Your demo has ended</h1>
<p>Your copy of Bookclerk and everything in it has been deleted. Bookclerk signs your store accounts out before it deletes a copy; if Amazon still lists a device named Bookclerk, you can remove it under <em>Manage Your Content and Devices</em>.</p>
<p><a href="/">Start a new demo</a></p>`,
    { siteUrl: p.siteUrl, card: true },
  );
}

/** Messages for the reasons a new session is refused. */
export const REFUSALS = Object.freeze({
  turnstile: "The check that you are a person did not pass. Please try again.",
  rate: "You have started several demos in the last hour. Please try again later.",
  busy: "The demo is busy right now. Please try again in a few minutes.",
  budget: "The demo has reached its limit for today. Please try again tomorrow.",
  origin: "That request did not come from this page. Please try again.",
});
