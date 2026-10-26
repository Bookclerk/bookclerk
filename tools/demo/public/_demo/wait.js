// Waiting page: follows the session (live.js) until the visitor's copy is
// ready, then enters it. Gives up with a message if starting fails or takes
// too long.
(() => {
  const status = document.getElementById("demo-wait-status");
  const started = Date.now();
  const throbber = document.getElementById("demo-wait-throbber");
  let done = false;
  const say = (text) => {
    if (status) status.textContent = text;
  };
  // Starting has stopped: the throbber must not suggest it is still going.
  const giveUp = (text) => {
    done = true;
    if (throbber) throbber.setAttribute("hidden", "");
    say(text);
  };
  window.bookclerkDemoLive.follow(
    (s) => {
      if (done) return;
      if (s.state === "ready") {
        done = true;
        window.location.replace("/_demo/enter");
      } else if (s.state !== "starting") {
        giveUp("Your copy of Bookclerk could not start. Please go back and try again.");
      }
    },
    { maxRetryMs: 2000 },
  );
  const slow = setInterval(() => {
    if (done) {
      clearInterval(slow);
      return;
    }
    const waited = Date.now() - started;
    if (waited > 180000) {
      clearInterval(slow);
      giveUp("This is taking longer than it should. Please go back and try again.");
    } else if (waited > 30000) {
      say("Still starting. Thanks for your patience.");
    }
  }, 1000);
})();
