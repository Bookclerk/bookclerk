// Waiting page: polls the session until the visitor's copy is ready, then
// enters it. Gives up with a message if starting fails or takes too long.
(() => {
  const status = document.getElementById("demo-wait-status");
  const started = Date.now();
  const say = (text) => {
    if (status) status.textContent = text;
  };
  const poll = async () => {
    let state = "unknown";
    try {
      const res = await fetch("/_demo/session", { headers: { Accept: "application/json" } });
      state = (await res.json()).state;
    } catch {
      // A missed poll is retried below.
    }
    if (state === "ready") {
      window.location.replace("/_demo/enter");
      return;
    }
    if (state === "failed" || state === "ended" || state === "none") {
      say("Your copy of Bookclerk could not start. Please go back and try again.");
      return;
    }
    if (Date.now() - started > 180000) {
      say("This is taking longer than it should. Please go back and try again.");
      return;
    }
    if (Date.now() - started > 30000) say("Still starting. Thanks for your patience.");
    setTimeout(poll, 2000);
  };
  poll();
})();
