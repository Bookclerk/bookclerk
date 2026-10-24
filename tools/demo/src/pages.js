// The demo Worker's own pages: the launch page with the Turnstile check, the
// waiting page, and the pages for a busy demo or an ended session. Styles
// and scripts are static files under public/_demo/, so the pages' CSP needs
// no inline code. Nothing loads from another origin except Turnstile.

/** Content-Security-Policy for the Worker's pages. Turnstile is the only outside origin. */
export const PAGE_CSP = [
  "default-src 'none'",
  "script-src 'self' https://challenges.cloudflare.com",
  "frame-src https://challenges.cloudflare.com",
  "style-src 'self'",
  "img-src 'self' data:",
  "connect-src 'self'",
  "form-action 'self'",
  "frame-ancestors 'none'",
  "base-uri 'none'",
].join("; ");

/** Headers every Worker page and proxied response carries. */
export const SECURITY_HEADERS = Object.freeze({
  "X-Content-Type-Options": "nosniff",
  "X-Frame-Options": "DENY",
  "Referrer-Policy": "same-origin",
  "Strict-Transport-Security": "max-age=31536000",
});

/**
 * Escapes text for HTML element content and quoted attributes.
 *
 * @param {string} s Text.
 * @returns {string} Escaped text.
 */
export function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

/**
 * Wraps page content in the shared document.
 *
 * @param {string} title Page title.
 * @param {string} body Body HTML (already escaped).
 * @param {{scripts?: string[]}} [opts] Script URLs to load, deferred.
 * @returns {string} The document.
 */
function documentHtml(title, body, opts = {}) {
  const scripts = (opts.scripts ?? []).map((src) => `<script src="${esc(src)}" defer></script>`).join("");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>${esc(title)}</title>
<link rel="stylesheet" href="/_demo/demo.css">
${scripts}
</head>
<body>
<main class="demo-card">
${body}
</main>
</body>
</html>`;
}

/**
 * An HTML response with the page CSP and security headers.
 *
 * @param {string} html Document.
 * @param {number} [status] HTTP status.
 * @param {Record<string, string>} [extra] More headers.
 * @returns {Response} The response.
 */
export function htmlResponse(html, status = 200, extra = {}) {
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

/** What the visitor is told before they start. */
const NOTICE = `
<ul class="demo-notes">
<li>You get your own private copy of Bookclerk for up to 30 minutes. Nobody else can see it, and it is deleted when you leave, after 10 minutes without activity, or when the time is up.</li>
<li>You can sign in to Audible, Libro.fm, Chirp or GraphicAudio to see the books you own and search the stores. Your sign-in goes only to that private copy and to the store.</li>
<li>The demo never downloads your books. Acquiring and downloading are turned off.</li>
<li>When the copy is deleted, Bookclerk signs your store accounts out first. Signing in to Audible adds a device named Bookclerk to your Amazon account, and signing out removes it. If a session ever ends without signing out, you can remove that device under <em>Manage Your Content and Devices</em> on Amazon.</li>
</ul>`;

/**
 * The launch page.
 *
 * @param {{siteKey: string | null, message?: string, siteUrl: string}} p
 *   Turnstile site key (null when the demo is not set up), an optional
 *   message to show above the button, and the website to link back to.
 * @returns {string} The document.
 */
export function launchPage(p) {
  const message = p.message ? `<p class="demo-message" role="alert">${esc(p.message)}</p>` : "";
  const form = p.siteKey
    ? `<form method="post" action="/_demo/start" class="demo-start">
<div class="cf-turnstile" data-sitekey="${esc(p.siteKey)}" data-action="demo_start"></div>
<button type="submit">Start the demo</button>
</form>`
    : `<p class="demo-message">The demo is not available right now.</p>`;
  const back = p.siteUrl ? `<p class="demo-small"><a href="${esc(p.siteUrl)}">Back to Bookclerk</a></p>` : "";
  return documentHtml(
    "Try Bookclerk",
    `<h1>Try Bookclerk</h1>
<p>Bookclerk keeps the audiobooks you buy from different stores in one library. Try it with your own store accounts.</p>
${NOTICE}
${message}
${form}
${back}`,
    { scripts: p.siteKey ? ["https://challenges.cloudflare.com/turnstile/v0/api.js"] : [] },
  );
}

/**
 * The page shown while the visitor's copy starts. /_demo/wait.js polls the
 * session and moves on when it is ready.
 *
 * @returns {string} The document.
 */
export function waitPage() {
  return documentHtml(
    "Starting your demo",
    `<h1>Starting your copy of Bookclerk</h1>
<p id="demo-wait-status" role="status">This takes about half a minute.</p>
<noscript><p>This page needs JavaScript to notice when your copy is ready. <a href="/_demo/enter">Continue</a></p></noscript>`,
    { scripts: ["/_demo/wait.js"] },
  );
}

/**
 * The page for a session that has ended (or never existed here).
 *
 * @param {{siteUrl: string}} p The website to link back to.
 * @returns {string} The document.
 */
export function endedPage(p) {
  const back = p.siteUrl ? ` or <a href="${esc(p.siteUrl)}">go back to Bookclerk</a>` : "";
  return documentHtml(
    "Demo ended",
    `<h1>Your demo has ended</h1>
<p>Your copy of Bookclerk and everything in it has been deleted. Bookclerk signs your store accounts out before it deletes a copy; if Amazon still lists a device named Bookclerk, you can remove it under <em>Manage Your Content and Devices</em>.</p>
<p><a href="/">Start a new demo</a>${back}.</p>`,
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
