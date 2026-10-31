// Shared layout enhancements: anchor clearance for the sticky header and a
// pre-release notice that collapses based on its actual number of lines.
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
