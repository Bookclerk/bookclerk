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
  for (const button of document.querySelectorAll(".lvl-toggle[data-level]")) {
    if (button.getAttribute("aria-pressed") !== "true")
      log?.classList.add(`hide-${button.dataset.level}`);
    button.addEventListener("click", () => {
      const on = button.getAttribute("aria-pressed") !== "true";
      button.setAttribute("aria-pressed", String(on));
      log?.classList.toggle(`hide-${button.dataset.level}`, !on);
    });
  }

  // Crate toggles hide every row (and folded run) from a crate, on top of
  // the level toggles. They stay hidden without this script.
  const crates = document.querySelector(".crate-tools");
  if (log && crates) {
    crates.hidden = false;
    for (const button of crates.querySelectorAll(".crate-toggle")) {
      button.addEventListener("click", () => {
        const on = button.getAttribute("aria-pressed") !== "true";
        button.setAttribute("aria-pressed", String(on));
        for (const el of log.querySelectorAll(":scope > .row, :scope > .run, .run > .row")) {
          if (el.dataset.crate === button.dataset.crate) el.hidden = !on;
        }
      });
    }
  }

  // Says something in a status element for a few seconds.
  const timers = new WeakMap();
  const say = (el, text) => {
    if (!el) return;
    el.textContent = text;
    clearTimeout(timers.get(el));
    timers.set(
      el,
      setTimeout(() => {
        el.textContent = "";
      }, 4000),
    );
  };
  const copy = async (text, el, done) => {
    try {
      await navigator.clipboard.writeText(text);
      say(el, done);
    } catch {
      say(el, "Could not copy: the browser refused clipboard access");
    }
  };

  // Event-log copy buttons put the rows on the clipboard as plain text, one
  // event per line with its fields separated by spaces, under a line naming
  // the report. "Copy shown" takes the rows on screen now (level and crate
  // toggles and folded runs leave the rest out); "Copy all" takes every
  // row; "Copy for issue" wraps the shown rows in Markdown with the
  // report's facts, ready to paste into a GitHub issue. The buttons stay
  // hidden without this script.
  const tools = document.querySelector(".copy-tools");
  const status = document.getElementById("copy-status");
  if (log && tools && navigator.clipboard) {
    tools.hidden = false;
    const part = (row, cls) => row.querySelector(`.${cls}`)?.textContent?.trim() ?? "";
    const line = (row) =>
      [part(row, "t"), part(row, "lv").padEnd(5), part(row, "tg"), part(row, "msg")]
        .filter((p) => p !== "")
        .join(" ");
    const shown = (row) =>
      typeof row.checkVisibility === "function" ? row.checkVisibility() : row.offsetParent !== null;
    for (const button of tools.querySelectorAll(".log-copy")) {
      button.addEventListener("click", () => {
        const all = button.dataset.copy === "all";
        const rows = [...log.querySelectorAll(".row")].filter((row) => all || shown(row));
        const d = log.dataset;
        const count = `${rows.length} ${rows.length === 1 ? "event" : "events"}`;
        if (button.dataset.copy === "issue") {
          const fence = "```";
          const facts = [
            `- Report: ${location.origin}/reports/${d.report}`,
            `- Version: ${d.version}`,
            `- Platform: ${d.platform}`,
            `- Trigger: ${d.trigger}`,
            d.signature ? `- Signature: \`${d.signature}\`` : "",
            d.route ? `- Route: \`${d.route}\`` : "",
          ].filter((f) => f !== "");
          const text = [...facts, "", `${fence}text`, ...rows.map(line), fence, ""].join("\n");
          void copy(text, status, `Copied ${count} as Markdown`);
          return;
        }
        const head = `Report ${d.report} · ${d.version} · ${d.platform} · trigger ${d.trigger}`;
        void copy([head, ...rows.map(line)].join("\n") + "\n", status, `Copied ${count}`);
      });
    }
  }

  // Buttons that copy a fixed text, such as the issue page's branch command;
  // hidden without this script or clipboard access.
  for (const button of document.querySelectorAll(".copy-text")) {
    if (!navigator.clipboard) continue;
    button.hidden = false;
    const near = button.parentElement?.querySelector("[data-copy-status]");
    button.addEventListener("click", () => {
      void copy(button.dataset.copyText ?? "", near, "Copied");
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
      // A redirect means the session ended: go to sign-in, which brings
      // this page back afterwards. Other failures wait for the next push.
      if (res.redirected) {
        signedOut();
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

  // The session ended (it expired, or someone signed out in another tab):
  // load sign-in with this page as the place to come back to, instead of
  // leaving stale numbers on screen whose next click would fail.
  function signedOut() {
    stop();
    location.replace(`/?next=${encodeURIComponent(location.pathname + location.search)}`);
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
      if (stopped) return;
      if (ev.code === 1008) {
        signedOut();
        return;
      }
      if (retries >= MAX_RETRIES) return;
      retries += 1;
      setTimeout(connect, Math.min(60000, 2000 * 2 ** (retries - 1)));
    });
  }

  // A tab that comes back after a long time away checks its session at once
  // (the hub only notices an expired session at its next push), so a page
  // whose session ran out goes to sign-in before anything is clicked.
  const STALE_MS = 15 * 60 * 1000;
  let hiddenAt = 0;
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      hiddenAt = Date.now();
      return;
    }
    if (pending || (hiddenAt && Date.now() - hiddenAt > STALE_MS)) changed();
  });
  connect();
})();
