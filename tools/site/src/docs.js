// Small enhancements for docs pages, served from the site itself (the CSP
// allows scripts from 'self' and Cloudflare's analytics beacon only). Pages
// work without it: it adds copy buttons to code blocks, copies a heading's
// link when it is clicked, offers Copy as Markdown, draws and follows the
// "On this page" rail, and adds a filter box to the sidebar. Its only
// network request is Copy as Markdown reading the page's own Markdown.

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

  /** Called with the id of the section being read whenever it changes. */
  const sectionListeners = [];

  /**
   * Copies a heading's link to the clipboard when its link icon is clicked;
   * the click still moves to the heading.
   */
  function copyHeadingLinks() {
    if (!navigator.clipboard || typeof navigator.clipboard.writeText !== "function") return;
    const anchors = document.querySelectorAll(".doc .heading-anchor");
    if (anchors.length === 0) return;
    // One polite status line tells screen readers whether the copy worked.
    const status = document.createElement("span");
    status.className = "visually-hidden";
    status.setAttribute("role", "status");
    document.body.append(status);
    let reset = 0;
    const show = (anchor, text, copied) => {
      for (const other of anchors) other.classList.remove("copied");
      anchor.classList.toggle("copied", copied);
      anchor.title = text;
      status.textContent = text;
      window.clearTimeout(reset);
      reset = window.setTimeout(() => {
        anchor.classList.remove("copied");
        anchor.removeAttribute("title");
        status.textContent = "";
      }, 2000);
    };
    for (const anchor of anchors) {
      anchor.addEventListener("click", () => {
        navigator.clipboard.writeText(anchor.href).then(
          () => show(anchor, "Link copied", true),
          () => show(anchor, "Could not copy the link", false),
        );
      });
    }
  }

  /**
   * Shows the Copy as Markdown button, which puts the page's Markdown
   * version on the clipboard.
   */
  function addCopyMarkdown() {
    const button = document.querySelector(".doc-copy-markdown");
    if (!button || !navigator.clipboard) return;
    const label = button.querySelector("span");
    const url = button.getAttribute("data-markdown");
    if (!label || !url) return;
    const read = () =>
      fetch(url, { headers: { Accept: "text/markdown" } }).then((response) => {
        if (!response.ok) throw new Error(`HTTP ${response.status}`);
        return response.text();
      });
    // Safari only lets a click write to the clipboard if the write starts in
    // the click, so the text is handed over as a promise where it can be.
    const write =
      typeof window.ClipboardItem === "function" && typeof navigator.clipboard.write === "function"
        ? () =>
            navigator.clipboard.write([
              new window.ClipboardItem({
                "text/plain": read().then((text) => new Blob([text], { type: "text/plain" })),
              }),
            ])
        : typeof navigator.clipboard.writeText === "function"
          ? () => read().then((text) => navigator.clipboard.writeText(text))
          : null;
    if (write === null) return;
    let reset = 0;
    const show = (text) => {
      label.textContent = text;
      window.clearTimeout(reset);
      reset = window.setTimeout(() => {
        label.textContent = "Copy as Markdown";
      }, 2000);
    };
    button.addEventListener("click", () => {
      write().then(
        () => show("Copied"),
        () => show("Could not copy"),
      );
    });
    label.setAttribute("aria-live", "polite");
    button.hidden = false;
  }

  /**
   * Draws the "On this page" rail: a line beside the links that steps in
   * beside subsections, with the section being read traced over it.
   */
  function drawTocRail() {
    const box = document.querySelector(".toc .toc-list");
    const list = box ? box.querySelector("ul") : null;
    if (!box || !list || typeof ResizeObserver !== "function") return;
    const ns = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(ns, "svg");
    svg.setAttribute("class", "toc-rail");
    svg.setAttribute("aria-hidden", "true");
    const base = document.createElementNS(ns, "path");
    base.setAttribute("class", "toc-rail-base");
    const active = document.createElementNS(ns, "path");
    active.setAttribute("class", "toc-rail-active");
    for (const path of [base, active]) {
      path.setAttribute("fill", "none");
      path.setAttribute("stroke-linecap", "round");
    }
    base.setAttribute("stroke-width", "1.5");
    active.setAttribute("stroke-width", "2");
    svg.append(base, active);
    box.prepend(svg);
    box.classList.add("has-rail");

    const links = [...list.querySelectorAll("li > a")];
    // Where each link's stretch of the rail starts along the path, and how long it is.
    let spans = [];
    let total = 0;
    let current = "";
    const highlight = () => {
      const at = links.findIndex((link) => link.getAttribute("href") === `#${current}`);
      const span = spans[at];
      if (!span) {
        active.style.strokeDasharray = `0 ${total + 1}`;
        return;
      }
      active.style.strokeDasharray = `${span.length} ${total + 1}`;
      active.style.strokeDashoffset = String(-span.start);
    };
    const draw = () => {
      const top = list.getBoundingClientRect().top;
      const items = links.map((link) => {
        const rect = link.getBoundingClientRect();
        return {
          x: link.parentElement.classList.contains("sub") ? 15 : 1,
          top: rect.top - top,
          bottom: rect.bottom - top,
        };
      });
      if (items.length === 0) return;
      // Each stretch ends 4px before its link does when the next one steps
      // in or out, and a curve joins the two.
      const parts = [];
      items.forEach((item, i) => {
        const next = items[i + 1];
        const start = i === 0 ? item.top : item.top + (items[i - 1].x === item.x ? 0 : 4);
        const end = next && next.x !== item.x ? item.bottom - 4 : item.bottom;
        parts.push({
          line: `L ${item.x} ${end}`,
          from: `${item.x} ${start}`,
          curve:
            next && next.x !== item.x
              ? `C ${item.x} ${item.bottom}, ${next.x} ${item.bottom}, ${next.x} ${item.bottom + 4}`
              : "",
        });
      });
      let d = `M ${parts[0].from}`;
      spans = [];
      for (const part of parts) {
        base.setAttribute("d", d);
        const start = d === `M ${parts[0].from}` ? 0 : base.getTotalLength();
        d += ` ${part.line}`;
        base.setAttribute("d", d);
        const end = base.getTotalLength();
        spans.push({ start, length: end - start });
        if (part.curve) d += ` ${part.curve}`;
      }
      base.setAttribute("d", d);
      active.setAttribute("d", d);
      total = base.getTotalLength();
      highlight();
    };
    new ResizeObserver(draw).observe(list);
    sectionListeners.push((id) => {
      current = id;
      highlight();
    });
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
      for (const listener of sectionListeners) listener(id);
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
  copyHeadingLinks();
  addCopyMarkdown();
  drawTocRail();
  trackSection();
  addSidebarFilter();
})();
