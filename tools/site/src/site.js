// Shared layout enhancements: anchor clearance for the sticky header, a
// pre-release notice that collapses based on its actual number of lines, and
// page dates shown in the reader's own time zone.
(() => {
  "use strict";

  /** The marketing or docs header, whose wrapped height can change with fonts. */
  const header = document.querySelector(".site-header");
  if (header && typeof ResizeObserver === "function") {
    /** Keeps section links clear of the actual sticky header at every width. */
    const updateHeader = () => {
      document.documentElement.style.setProperty(
        "--site-header-height",
        `${header.getBoundingClientRect().height}px`,
      );
    };
    /** Tracks wrapping and font changes without changing the header itself. */
    const headerObserver = new ResizeObserver(updateHeader);
    headerObserver.observe(header);
    updateHeader();
  }

  /** The optional pre-release notice; released pages have none. */
  const ribbon = document.querySelector(".ribbon");
  /** The visible badge, explanation and detail link. */
  const content = ribbon?.querySelector(".ribbon-content");
  if (!content || typeof ResizeObserver !== "function") return;

  /** An inaccessible full-size copy, unaffected by the visible collapse. */
  const measure = content.cloneNode(true);
  measure.classList.remove("ribbon-content");
  measure.classList.add("ribbon-measure");
  measure.setAttribute("aria-hidden", "true");
  measure.inert = true;
  ribbon.append(measure);
  ribbon.setAttribute("data-ribbon-ready", "");

  /**
   * Collapses a notice that occupies more than two lines, including flex rows.
   * Two separate rows include one row gap; padding is not part of the text.
   */
  function update() {
    const style = getComputedStyle(measure);
    const height =
      measure.getBoundingClientRect().height -
      parseFloat(style.paddingTop) -
      parseFloat(style.paddingBottom);
    const twoLines = 2 * parseFloat(style.lineHeight) + parseFloat(style.rowGap);
    // Ignore subpixel rounding at the exact two-line boundary.
    ribbon.toggleAttribute("data-compact", height > twoLines + 1);
  }

  /** Tracks width, zoom and font-metric changes without observing our own collapse. */
  const observer = new ResizeObserver(update);
  observer.observe(measure);
  update();
  // A loaded web font can change line breaks without changing the viewport.
  if (document.fonts) document.fonts.ready.then(update);
})();

// Page dates ("Last updated", the /sitemap list) are rendered as the date in
// UTC, so pages read the same without JavaScript. A <time data-local-date>
// carries the instant it names (`YYYY-MM-DDTHH:MM:SSZ`); this rewrites its
// text to the date in the browser's time zone, in the page's language, and
// its tooltip to the full local time. Nothing is stored or sent: the time
// zone is only read by Intl, here.
(() => {
  "use strict";

  /** The instants the server writes; anything else is left as it is. */
  const INSTANT = /^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$/;

  if (typeof Intl !== "object" || typeof Intl.DateTimeFormat !== "function") return;
  const times = document.querySelectorAll("time[data-local-date]");
  if (times.length === 0) return;

  /** The page's language (`en`), so the text matches the rest of the page. */
  const locale = document.documentElement.lang || undefined;
  let day;
  let full;
  try {
    day = new Intl.DateTimeFormat(locale, { dateStyle: "medium" });
    full = new Intl.DateTimeFormat(locale, { dateStyle: "medium", timeStyle: "short" });
  } catch {
    // A browser without dateStyle keeps the UTC date.
    return;
  }

  for (const time of times) {
    const stamp = time.getAttribute("datetime") || "";
    if (!INSTANT.test(stamp)) continue;
    const at = new Date(stamp);
    // Browsers roll an impossible day (Feb 30) over to the next month.
    if (Number.isNaN(at.getTime()) || at.toISOString() !== stamp.replace("Z", ".000Z")) continue;
    time.textContent = day.format(at);
    time.setAttribute("title", full.format(at));
  }
})();
