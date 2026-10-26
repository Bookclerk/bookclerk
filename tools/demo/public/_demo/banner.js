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
  // Start over ends the session itself; its answer clears the cookies, so
  // the "ended" news it causes must not navigate away from that answer.
  let startingOver = false;
  form.addEventListener("submit", () => {
    startingOver = true;
  });

  const get = document.createElement("a");
  get.textContent = "Get Bookclerk";
  get.rel = "noopener";
  get.hidden = true;

  bar.append(label, left, spacer, form, get);
  const mount = () => document.body.prepend(bar);
  if (document.body) mount();
  else document.addEventListener("DOMContentLoaded", mount);

  // The countdown runs from the seconds left as the server counts them,
  // against this page's monotonic clock, so the computer's own clock does not
  // matter. It is display only: the session's own timer ends the session.
  let deadline = 0;
  let timer = 0;
  let leaving = 0;
  const tick = () => {
    if (!deadline) return;
    const s = Math.max(0, Math.ceil((deadline - performance.now()) / 1000));
    const m = Math.floor(s / 60);
    left.textContent = s > 0 ? `${m}:${String(s % 60).padStart(2, "0")} left` : "Time is up";
    // The session tells the page when it has ended; leave anyway if that
    // news does not arrive soon after the time is up.
    if (s === 0 && !leaving && !startingOver) leaving = setTimeout(() => window.location.assign("/"), 10000);
  };
  window.bookclerkDemoLive.follow((s) => {
    if (s.siteUrl) {
      get.href = s.siteUrl;
      get.hidden = false;
    }
    if (startingOver) return;
    if (s.state !== "starting" && s.state !== "ready") {
      // Ended here or in another tab (Start over, the time limit, idle).
      left.textContent = "Demo ended";
      window.location.assign("/");
      return;
    }
    deadline = performance.now() + s.remaining * 1000;
    tick();
    if (!timer) timer = setInterval(tick, 1000);
  });
  // Background tabs tick late; catch up as soon as the tab is shown again.
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "visible") tick();
  });
})();
