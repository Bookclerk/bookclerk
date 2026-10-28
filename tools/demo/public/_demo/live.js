// Follows the visitor's demo session for the waiting page and the banner.
//
// The session pushes its status over a WebSocket (/_demo/live): its state,
// the seconds left by the server's clock, and the website link. When the
// socket cannot open or drops, the page asks /_demo/session instead and
// tries the socket again with a growing delay, so the status still arrives,
// just later. A page coming back into view asks for the status again,
// because background tabs run their timers late.
//
// Usage: bookclerkDemoLive.follow(onStatus, { maxRetryMs }), where onStatus
// receives { state, remaining, siteUrl } and maxRetryMs caps the delay
// between fallback polls (the waiting page keeps it short). It stops after a
// state that ends the session ("ending", "ended", "failed" or "none").
(() => {
  const OVER = new Set(["ending", "ended", "failed", "none"]);
  // The session closed this socket to make room for newer tabs (live.ts
  // CLOSE_TOO_MANY): poll instead of taking a socket back from them.
  const CLOSE_TOO_MANY = 4001;
  // A proxy can stall the upgrade without closing it; give up on a socket
  // that has not opened by then so the fallback polls start.
  const HANDSHAKE_MS = 5000;
  const STATES = new Set(["starting", "ready", ...OVER]);

  // Keeps only the fields the pages use, with their expected types.
  const parse = (data) => {
    try {
      const s = JSON.parse(data);
      if (!s || typeof s !== "object" || !STATES.has(s.state)) return null;
      const remaining = Number.isFinite(s.remaining) ? Math.max(0, s.remaining) : 0;
      const siteUrl =
        typeof s.siteUrl === "string" && s.siteUrl.startsWith("https://") ? s.siteUrl : "";
      return { state: s.state, remaining, siteUrl };
    } catch {
      return null;
    }
  };

  const follow = (onStatus, options = {}) => {
    const maxRetryMs = options.maxRetryMs ?? 30000;
    let socket = null;
    let over = false;
    let pollOnly = false;
    let retryMs = Math.min(1000, maxRetryMs);
    let retryTimer = 0;

    const deliver = (status) => {
      if (!status || over) return;
      if (OVER.has(status.state)) over = true;
      onStatus(status);
      if (over && socket) socket.close();
    };

    const poll = async () => {
      try {
        const res = await fetch("/_demo/session", { headers: { Accept: "application/json" } });
        deliver(parse(await res.text()));
      } catch {
        // The next retry asks again.
      }
    };

    const connect = () => {
      if (over) return;
      let opened = false;
      let closed = false;
      try {
        const scheme = location.protocol === "https:" ? "wss:" : "ws:";
        socket = new WebSocket(`${scheme}//${location.host}/_demo/live`);
      } catch {
        socket = null;
        retry();
        return;
      }
      const current = socket;
      setTimeout(
        () => {
          if (!opened && !closed) current.close();
        },
        Math.min(HANDSHAKE_MS, maxRetryMs),
      );
      socket.addEventListener("open", () => {
        opened = true;
      });
      socket.addEventListener("message", (event) => {
        if (typeof event.data === "string") deliver(parse(event.data));
      });
      socket.addEventListener("close", (event) => {
        closed = true;
        socket = null;
        if (over) return;
        if (event.code === CLOSE_TOO_MANY) {
          pollOnly = true;
          retryMs = maxRetryMs;
        } else if (opened) {
          // A socket that had worked: try it again soon.
          retryMs = Math.min(1000, maxRetryMs);
        }
        retry();
      });
    };

    const retry = () => {
      if (over || retryTimer) return;
      retryTimer = setTimeout(() => {
        retryTimer = 0;
        poll();
        if (pollOnly) retry();
        else connect();
      }, retryMs);
      retryMs = Math.min(maxRetryMs, retryMs * 2);
    };

    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState !== "visible" || over) return;
      if (socket && socket.readyState === WebSocket.OPEN) socket.send("sync");
      else poll();
    });

    connect();
  };

  window.bookclerkDemoLive = { follow };
})();
