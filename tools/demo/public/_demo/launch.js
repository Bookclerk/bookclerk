// Renders the launch page's Turnstile check, and renders it again when a
// page left open comes back without a usable one.
//
// Size: Turnstile's normal widget is a fixed 300px wide, which does not fit
// the start card on the narrowest phones, so screens below 366px (300px plus
// the page and card padding) get the compact widget instead. Turnstile's
// api.js calls bookclerkDemoTurnstile once it has loaded
// (`render=explicit&onload=`).
//
// Coming back: a page restored from the back/forward cache (Safari keeps
// pages there even with `Cache-Control: no-store`) or shown again after a
// long time in a background tab can hold a widget whose challenge frame was
// torn down, or whose answer expired (Turnstile answers last 300 seconds),
// so the visitor sees no check at all. When the page is restored from the
// cache, or shown after STALE_MS hidden, every widget is removed and
// rendered afresh; when it is shown sooner, only a widget that is missing,
// collapsed or expired is. If Turnstile's script itself is gone by then,
// the page reloads, which fetches a fresh page (it is never cached).
(() => {
  const NARROW = "(max-width: 365px)";
  /** How long hidden before the check is redone: under the answer's 300 s life. */
  const STALE_MS = 4 * 60 * 1000;
  /** Each rendered container and its Turnstile widget id. */
  const widgets = new Map();
  /** When the page was last hidden (Date.now(), which keeps counting while asleep), or null. */
  let hiddenAt = null;

  /** Renders one container's widget, replacing any it had. */
  const render = (el) => {
    const old = widgets.get(el);
    if (old !== undefined) window.turnstile.remove(old);
    el.replaceChildren();
    const size = window.matchMedia(NARROW).matches ? "compact" : "normal";
    const id = window.turnstile.render(el, {
      sitekey: el.dataset.sitekey,
      action: el.dataset.action,
      size,
    });
    widgets.set(el, id);
  };

  /** Whether a container no longer shows a usable widget. */
  const unusable = (el) => {
    const id = widgets.get(el);
    return id === undefined || el.offsetHeight === 0 || window.turnstile.isExpired(id);
  };

  /**
   * Checks every widget after the page comes back.
   *
   * @param stale - Whether to render every widget afresh, usable or not.
   */
  const recheck = (stale) => {
    if (!window.turnstile) {
      // api.js has not run. On a fresh load it is still on its way; on a
      // page that comes back it is not coming, so fetch the page again.
      if (stale) window.location.reload();
      return;
    }
    for (const el of document.querySelectorAll(".cf-turnstile")) {
      if (stale || unusable(el)) render(el);
    }
  };

  window.bookclerkDemoTurnstile = () => {
    for (const el of document.querySelectorAll(".cf-turnstile")) render(el);
  };

  window.addEventListener("pageshow", (event) => {
    if (!event.persisted) return;
    hiddenAt = null;
    recheck(true);
  });
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState !== "visible") {
      hiddenAt = Date.now();
      return;
    }
    const stale = hiddenAt !== null && Date.now() - hiddenAt >= STALE_MS;
    hiddenAt = null;
    recheck(stale);
  });
})();
