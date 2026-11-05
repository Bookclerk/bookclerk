// Applies the saved light/dark theme before the first paint. It is a file
// rather than an inline script so the app's Content-Security-Policy can keep
// `script-src 'self'`.
(function () {
  var pref = "system";
  try {
    pref = localStorage.getItem("bookclerk-theme") || "system";
  } catch (e) {}
  var dark =
    pref === "dark" ||
    (pref !== "light" &&
      window.matchMedia &&
      window.matchMedia("(prefers-color-scheme: dark)").matches);
  if (dark) {
    document.documentElement.classList.add("dark");
    document.documentElement.style.colorScheme = "dark";
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", "#121c26");
  } else {
    document.documentElement.style.colorScheme = "light";
  }
})();
