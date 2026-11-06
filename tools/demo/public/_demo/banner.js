// The demo bar on top of the app: time left, Start over, Get Bookclerk, and
// a "Demo" button that shows the Owner password for elevating in Settings,
// which the bar also fills in on the app's Elevate to Operator form.
// Added by the demo Worker; the app itself carries no demo code.
(() => {
  // VISITOR_PASSWORD in src/lifecycle.ts (test/pages.test.ts checks they match).
  const PASSWORD = "bookclerk-demo";
  const bar = document.createElement("div");
  bar.id = "bookclerk-demo-banner";
  bar.setAttribute("role", "region");
  bar.setAttribute("aria-label", "Bookclerk demo");

  // "Bookclerk demo" on wide screens, "Demo" on phones (banner.css). It is a
  // button that opens the tips panel below the bar.
  const label = document.createElement("button");
  label.type = "button";
  label.className = "demo-label";
  label.setAttribute("aria-expanded", "false");
  label.setAttribute("aria-controls", "bookclerk-demo-tips");
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

  const tips = document.createElement("div");
  tips.id = "bookclerk-demo-tips";
  tips.hidden = true;
  const intro = document.createElement("p");
  intro.textContent =
    "You are this library's Owner. To act as another user, open Settings, then Account, " +
    "choose Elevate to Operator (the password is filled in for you), and pick Impersonate " +
    "in User Management. " +
    "Elevation lasts 15 minutes.";
  const pass = document.createElement("p");
  const passLabel = document.createElement("span");
  passLabel.textContent = "Password: ";
  const code = document.createElement("code");
  code.textContent = PASSWORD;
  pass.append(passLabel, code);
  tips.append(intro, pass);
  label.addEventListener("click", () => {
    tips.hidden = !tips.hidden;
    label.setAttribute("aria-expanded", String(!tips.hidden));
  });

  // Settings > Account shows an "Elevate to Operator" form with an empty
  // password box. Fill it with the demo password once, the way typing would,
  // so React sees the value; a box the visitor has typed in is left alone.
  const valueSetter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
  const fillElevation = () => {
    for (const input of document.querySelectorAll('form input[type="password"]')) {
      if (input.dataset.demoFilled || input.value) continue;
      const submit = input.form?.querySelector('button[type="submit"]');
      if (!submit || !/Elevat/.test(submit.textContent || "")) continue;
      input.dataset.demoFilled = "1";
      if (valueSetter) valueSetter.call(input, PASSWORD);
      else input.value = PASSWORD;
      input.dispatchEvent(new Event("input", { bubbles: true }));
    }
  };
  new MutationObserver(fillElevation).observe(document.documentElement, {
    childList: true,
    subtree: true,
  });

  bar.append(label, left, spacer, form, get);
  const mount = () => {
    document.body.prepend(bar);
    bar.after(tips);
  };
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
    if (s === 0 && !leaving && !startingOver)
      leaving = setTimeout(() => window.location.assign("/"), 10000);
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
