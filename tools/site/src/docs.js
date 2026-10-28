// Small enhancements for docs pages, served from the site itself (the CSP
// allows scripts from 'self' and Cloudflare's analytics beacon only). Pages
// work without it: it adds copy buttons to code blocks, marks the section
// being read in the "On this page" rail, and adds a filter box to the
// sidebar. It makes no network requests.

(() => {
  "use strict";

  /**
   * Whether a shell line goes on to the next one: it ends in an unescaped
   * `\` that is not inside single quotes or a `#` comment. Quotes and
   * escapes are followed, so `echo "a # b" \` continues, `ls;# note \`
   * does not, and `\\` at the end is a literal backslash.
   *
   * @param {string} line - One line of a command.
   * @returns {boolean} True when the command continues on the next line.
   */
  function lineContinues(line) {
    let quote = "";
    for (let i = 0; i < line.length; i++) {
      const c = line[i];
      if (c === "\\" && quote !== "'") {
        if (i === line.length - 1) return true;
        i += 1;
      } else if (quote !== "") {
        if (c === quote) quote = "";
      } else if (c === "'" || c === '"') {
        quote = c;
      } else if (c === "#" && (i === 0 || /[\s;&|()<>]/.test(line[i - 1]))) {
        return false;
      }
    }
    return false;
  }

  /**
   * The text a Copy button puts on the clipboard: for a terminal session
   * (lines starting with a `$ ` prompt), only the commands without their
   * prompts, with the lines that continue a command (see lineContinues);
   * otherwise the whole block.
   *
   * @param {string} text - The code block's text.
   * @returns {string} What to copy.
   */
  function copyText(text) {
    const lines = text.replace(/\n$/, "").split("\n");
    if (!lines.some((line) => line.startsWith("$ "))) return lines.join("\n");
    const commands = [];
    let continued = false;
    for (const line of lines) {
      if (line.startsWith("$ ")) commands.push(line.slice(2));
      else if (continued) commands.push(line);
      else continue;
      continued = lineContinues(line);
    }
    return commands.join("\n");
  }

  /**
   * Adds a Copy button to every code block that the clipboard can take.
   */
  function addCopyButtons() {
    if (!navigator.clipboard || typeof navigator.clipboard.writeText !== "function") return;
    for (const block of document.querySelectorAll(".doc .code")) {
      const pre = block.querySelector("pre");
      if (!pre) continue;
      const button = document.createElement("button");
      button.type = "button";
      button.className = "copy";
      button.textContent = "Copy";
      button.setAttribute("aria-label", "Copy this code");
      let reset = 0;
      button.addEventListener("click", () => {
        navigator.clipboard.writeText(copyText(pre.innerText)).then(
          () => {
            button.textContent = "Copied";
            window.clearTimeout(reset);
            reset = window.setTimeout(() => {
              button.textContent = "Copy";
            }, 2000);
          },
          () => {
            button.textContent = "Select and copy";
          },
        );
      });
      block.append(button);
    }
  }

  /**
   * Marks the "On this page" link of the section at the top of the window
   * with aria-current="location", in the rail and in the phone menu.
   */
  function trackSection() {
    const links = [...document.querySelectorAll('.toc a[href^="#"], .menu-toc a[href^="#"]')];
    if (links.length === 0) return;
    const headings = [...document.querySelectorAll(".doc h2[id], .doc h3[id]")];
    let current = "";
    const mark = (id) => {
      if (id === current) return;
      current = id;
      for (const link of links) {
        if (link.getAttribute("href") === `#${id}`) link.setAttribute("aria-current", "location");
        else link.removeAttribute("aria-current");
      }
    };
    const update = () => {
      // The last heading above a line a quarter of the way down the window.
      const line = window.innerHeight / 4;
      let id = headings[0] ? headings[0].id : "";
      for (const heading of headings) {
        if (heading.getBoundingClientRect().top <= line) id = heading.id;
        else break;
      }
      mark(id);
    };
    // At most one update per frame while scrolling.
    let queued = false;
    const schedule = () => {
      if (queued) return;
      queued = true;
      window.requestAnimationFrame(() => {
        queued = false;
        update();
      });
    };
    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", schedule, { passive: true });
    window.addEventListener("hashchange", schedule);
    update();
  }

  /**
   * Adds a box above the sidebar groups that hides pages whose title does
   * not contain what is typed. "/" focuses it, Escape clears it.
   */
  function addSidebarFilter() {
    const nav = document.querySelector(".docs-nav");
    const groups = nav ? [...nav.querySelectorAll(".nav-group")] : [];
    if (!nav || groups.length === 0) return;
    const wasOpen = new Map(groups.map((group) => [group, group.open]));
    const box = document.createElement("div");
    box.className = "nav-filter";
    const input = document.createElement("input");
    input.type = "search";
    input.placeholder = "Filter pages";
    input.setAttribute("aria-label", "Filter pages");
    input.autocomplete = "off";
    const hint = document.createElement("kbd");
    hint.textContent = "/";
    hint.setAttribute("aria-hidden", "true");
    box.append(input, hint);
    const empty = document.createElement("p");
    empty.className = "nav-empty";
    empty.textContent = "No page matches.";
    empty.hidden = true;
    groups[0].before(box);
    groups[groups.length - 1].after(empty);
    const apply = () => {
      const query = input.value.trim().toLowerCase();
      let shown = 0;
      for (const group of groups) {
        let groupShown = 0;
        for (const item of group.querySelectorAll("li")) {
          const match = query === "" || item.textContent.toLowerCase().includes(query);
          item.hidden = !match;
          if (match) groupShown++;
        }
        group.hidden = groupShown === 0;
        group.open = query === "" ? (wasOpen.get(group) ?? false) : groupShown > 0;
        shown += groupShown;
      }
      empty.hidden = shown > 0;
    };
    input.addEventListener("input", apply);
    input.addEventListener("keydown", (event) => {
      if (event.key === "Escape" && input.value !== "") {
        input.value = "";
        apply();
      }
    });
    for (const group of groups) {
      group.addEventListener("toggle", () => {
        if (input.value.trim() === "") wasOpen.set(group, group.open);
      });
    }
    document.addEventListener("keydown", (event) => {
      const target = event.target;
      const typing =
        target instanceof HTMLElement &&
        (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName));
      if (event.key !== "/" || typing || event.ctrlKey || event.metaKey || event.altKey) return;
      if (input.offsetParent === null) return;
      event.preventDefault();
      input.focus();
    });
  }

  addCopyButtons();
  trackSection();
  addSidebarFilter();
})();
