// The demo bar on top of the app: time left, Start over, Get Bookclerk.
// Added by the demo Worker; the app itself carries no demo code.
(() => {
  const bar = document.createElement("div");
  bar.id = "bookclerk-demo-banner";
  bar.setAttribute("role", "region");
  bar.setAttribute("aria-label", "Bookclerk demo");

  // "Bookclerk demo" on wide screens, "Demo" on phones (banner.css).
  const label = document.createElement("strong");
  const long = document.createElement("span");
  long.className = "demo-long";
  long.textContent = "Bookclerk demo";
  const short = document.createElement("span");
  short.className = "demo-short";
  short.textContent = "Demo";
  label.append(long, short);
  const left = document.createElement("span");
  left.className = "demo-left";
  left.setAttribute("aria-live", "polite");
  const spacer = document.createElement("span");
  spacer.className = "demo-spacer";

  const form = document.createElement("form");
  form.method = "post";
  form.action = "/_demo/end";
  const over = document.createElement("button");
  over.type = "submit";
  over.textContent = "Start over";
  form.append(over);

  const get = document.createElement("a");
  get.textContent = "Get Bookclerk";
  get.rel = "noopener";
  get.hidden = true;

  bar.append(label, left, spacer, form, get);
  const mount = () => document.body.prepend(bar);
  if (document.body) mount();
  else document.addEventListener("DOMContentLoaded", mount);

  let endsAt = 0;
  const tick = () => {
    if (!endsAt) return;
    const s = Math.max(0, endsAt - Math.floor(Date.now() / 1000));
    const m = Math.floor(s / 60);
    left.textContent = s > 0 ? `${m}:${String(s % 60).padStart(2, "0")} left` : "Time is up";
    if (s === 0) window.location.assign("/");
  };
  fetch("/_demo/session", { headers: { Accept: "application/json" } })
    .then((r) => r.json())
    .then((s) => {
      endsAt = Number(s.endsAt) || 0;
      if (typeof s.siteUrl === "string" && s.siteUrl.startsWith("https://")) {
        get.href = s.siteUrl;
        get.hidden = false;
      }
      tick();
      setInterval(tick, 1000);
    })
    .catch(() => {
      left.textContent = "";
    });
})();
