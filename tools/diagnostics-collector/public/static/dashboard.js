// Small progressive touches for the diagnostics dashboard. Every page works
// without this script: filter menus also have an Apply button inside
// <noscript>, and the event log shows every level until the script hides the
// levels whose toggle starts off (debug and trace).

document.addEventListener("DOMContentLoaded", () => {
  // Filter menus apply as soon as a value is picked.
  for (const select of document.querySelectorAll("select.autosubmit")) {
    select.addEventListener("change", () => select.form?.submit());
  }

  // Event-log level toggles show and hide rows by level.
  const log = document.getElementById("log");
  for (const button of document.querySelectorAll(".lvl-toggle")) {
    if (button.getAttribute("aria-pressed") !== "true") log?.classList.add(`hide-${button.dataset.level}`);
    button.addEventListener("click", () => {
      const on = button.getAttribute("aria-pressed") !== "true";
      button.setAttribute("aria-pressed", String(on));
      log?.classList.toggle(`hide-${button.dataset.level}`, !on);
    });
  }
});
