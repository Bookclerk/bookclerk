// Small progressive touches for the diagnostics dashboard. Every page works
// without this script: filter menus also have an Apply button inside
// <noscript>, and the event log shows every level until the script hides the
// levels whose toggle starts off (debug and trace). Live updates are the
// same: without the script a page shows what it had when it loaded.

document.addEventListener("DOMContentLoaded", () => {
  // Filter menus apply as soon as a value is picked.
  for (const select of document.querySelectorAll("select.autosubmit")) {
    select.addEventListener("change", () => select.form?.submit());
  }

  // Event-log level toggles show and hide rows by level.
  const log = document.getElementById("log");
  for (const button of document.querySelectorAll(".lvl-toggle")) {
    if (button.getAttribute("aria-pressed") !== "true")
      log?.classList.add(`hide-${button.dataset.level}`);
    button.addEventListener("click", () => {
      const on = button.getAttribute("aria-pressed") !== "true";
      button.setAttribute("aria-pressed", String(on));
      log?.classList.toggle(`hide-${button.dataset.level}`, !on);
    });
  }
});

// Live updates. The page opens one WebSocket to /live; the server sends the
// text "changed" when reports or issue links change (it never sends data).
// The page then fetches its own URL once and swaps in each element marked
// data-live, so numbers and links update without a reload and without
// polling. Pushes that arrive while a fetch runs, or within MIN_GAP_MS of
// the last one, collapse into one more fetch; a hidden tab waits until it
// is shown again.
(() => {
  const indicator = () => document.getElementById("live");
  if (!("WebSocket" in window) || !document.querySelector("[data-live]")) return;

  const MIN_GAP_MS = 3000;
  const MAX_RETRIES = 6;
  let pending = false;
  let busy = false;
  let lastAt = 0;
  let retries = 0;
  let stopped = false;

  async function refresh() {
    if (busy || document.hidden || stopped) return;
    const wait = lastAt + MIN_GAP_MS - Date.now();
    if (wait > 0) {
      setTimeout(refresh, wait);
      return;
    }
    busy = true;
    pending = false;
    lastAt = Date.now();
    try {
      const res = await fetch(location.href, {
        headers: { "Bookclerk-Live": "1" },
        credentials: "same-origin",
      });
      // A redirect means the session ended; stop rather than show the
      // sign-in page. Other failures wait for the next push.
      if (res.redirected) {
        stop();
        return;
      }
      if (!res.ok) return;
      const fresh = new DOMParser().parseFromString(await res.text(), "text/html");
      for (const el of document.querySelectorAll("[data-live]")) {
        const next = fresh.querySelector(`[data-live="${CSS.escape(el.dataset.live)}"]`);
        if (next) el.replaceWith(document.adoptNode(next));
      }
    } catch {
      // Network blip; the next push tries again.
    } finally {
      busy = false;
      if (pending) refresh();
    }
  }

  function changed() {
    pending = true;
    refresh();
  }

  let socket = null;

  function stop() {
    stopped = true;
    socket?.close(1000);
    const dot = indicator();
    if (dot) dot.hidden = true;
  }

  function connect() {
    const ws = new WebSocket(
      `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/live`,
    );
    socket = ws;
    ws.addEventListener("open", () => {
      const dot = indicator();
      if (dot) dot.hidden = false;
      // Catch up on anything missed while disconnected.
      if (retries > 0) changed();
      retries = 0;
    });
    ws.addEventListener("message", (ev) => {
      if (ev.data === "changed") changed();
    });
    ws.addEventListener("close", (ev) => {
      const dot = indicator();
      if (dot) dot.hidden = true;
      // 1008: the session ended. Otherwise reconnect with backoff, a few times.
      if (stopped || ev.code === 1008 || retries >= MAX_RETRIES) return;
      retries += 1;
      setTimeout(connect, Math.min(60000, 2000 * 2 ** (retries - 1)));
    });
  }

  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && pending) refresh();
  });
  connect();
})();
