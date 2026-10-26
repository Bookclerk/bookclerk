// Renders the launch page's Turnstile check. Turnstile's normal widget is a
// fixed 300px wide, which does not fit the start card on the narrowest
// phones, so screens below 366px (300px plus the page and card padding) get
// the compact widget instead. Turnstile's api.js calls
// bookclerkDemoTurnstile once it has loaded (`render=explicit&onload=`).
(() => {
  const NARROW = "(max-width: 365px)";

  window.bookclerkDemoTurnstile = () => {
    const size = window.matchMedia(NARROW).matches ? "compact" : "normal";
    for (const el of document.querySelectorAll(".cf-turnstile")) {
      window.turnstile.render(el, { sitekey: el.dataset.sitekey, action: el.dataset.action, size });
    }
  };
})();
