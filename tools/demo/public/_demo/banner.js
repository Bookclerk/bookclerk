// The demo bar on top of the app: time left, Start over, Get Bookclerk.
// Added by the demo Worker; the app itself carries no demo code.
(() => {
  const bar = document.createElement("div");
  bar.id = "bookclerk-demo-banner";
  bar.setAttribute("role", "region");
  bar.setAttribute("aria-label", "Bookclerk demo");

  const label = document.createElement("strong");
  label.textContent = "Bookclerk demo";
  const left = document.createElement("span");
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
    left.textContent = s > 0 ? `${m} min ${String(s % 60).padStart(2, "0")} s left` : "Time is up";
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
