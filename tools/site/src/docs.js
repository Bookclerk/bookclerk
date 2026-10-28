// Small enhancements for docs pages, served from the site itself (the CSP
// allows scripts from 'self' and Cloudflare's analytics beacon only). Pages
// work without it: it adds copy buttons to code blocks, copies a heading's
// link when it is clicked, offers Copy as Markdown, draws and follows the
// "On this page" rail, closes the phone menus like menus, shows example
// responses as tabs by status with an Example/Schema view, example requests
// as tabs by language, and adds a filter box to the sidebar. Its only
// network requests are same-origin: Copy as Markdown reads the page's own
// Markdown, and the filter reads the docs version's search index. The only
// things it keeps in the browser are the request sample language and the
// response view last picked (localStorage), which the privacy page lists as
// display preferences.

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

    // Links and the path labels between nested routes, each one row of the rail.
    const rows = [...list.querySelectorAll("li > a, li > .toc-label")];
    // Where each link's stretch of the rail starts along the path, and how long it is.
    let spans = [];
    let total = 0;
    let current = "";
    const highlight = () => {
      const at = rows.findIndex((row) => row.getAttribute("href") === `#${current}`);
      const span = spans[at];
      if (!span) {
        active.style.strokeDasharray = `0 ${total + 1}`;
        return;
      }
      active.style.strokeDasharray = `${span.length} ${total + 1}`;
      active.style.strokeDashoffset = String(-span.start);
    };
    // The rail scrolls on its own when its routes outgrow the window; keep
    // the current one in view without moving the page.
    const pane = box.closest(".toc");
    const reveal = () => {
      const row = rows.find((r) => r.getAttribute("href") === `#${current}`);
      if (!pane || !row || pane.scrollHeight <= pane.clientHeight) return;
      const view = pane.getBoundingClientRect();
      const rect = row.getBoundingClientRect();
      const margin = 48;
      if (rect.top < view.top + margin) pane.scrollTop -= view.top + margin - rect.top;
      else if (rect.bottom > view.bottom - margin)
        pane.scrollTop += rect.bottom - (view.bottom - margin);
    };
    const draw = () => {
      const top = list.getBoundingClientRect().top;
      const items = rows.map((row) => {
        const rect = row.getBoundingClientRect();
        const depth = Number((/\bdepth-(\d)\b/.exec(row.parentElement.className) || [])[1] || 0);
        return { x: 1 + 14 * depth, top: rect.top - top, bottom: rect.bottom - top };
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
      // Also runs when the rail comes back into view after a resize.
      reveal();
    };
    new ResizeObserver(draw).observe(list);
    sectionListeners.push((id) => {
      current = id;
      highlight();
      reveal();
    });
  }

  /**
   * Marks the "On this page" link of the section at the top of the window
   * with aria-current="location", in the rail and in the phone menu.
   */
  function trackSection() {
    const links = [...document.querySelectorAll('.toc a[href^="#"], .menu-toc a[href^="#"]')];
    if (links.length === 0) return;
    // Only headings the rail lists: API pages leave their per-route
    // subsections (Responses, Request body) out of it.
    const listed = new Set(links.map((link) => link.getAttribute("href").slice(1)));
    const headings = [...document.querySelectorAll(".doc h2[id], .doc h3[id]")].filter((h) =>
      listed.has(h.id),
    );
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

  /** Most pages the sidebar filter lists. */
  const MAX_RESULTS = 30;

  /** Most matching headings listed under one page. */
  const MAX_HEADINGS = 4;

  /**
   * Reads the docs version's search index (worker.ts `serveSearchIndex`),
   * keeping only well-formed entries.
   *
   * @param {string} url - The index's URL path.
   * @returns {Promise<Array<{section: string, title: string, url: string, headings: Array<{text: string, url: string}>}>>}
   *   Its pages; rejects when it cannot be read.
   */
  async function loadSearchIndex(url) {
    const response = await fetch(url, { headers: { Accept: "application/json" } });
    if (!response.ok) throw new Error(`search index: ${response.status}`);
    const data = await response.json();
    const isText = (v) => typeof v === "string";
    const sameSite = (v) => isText(v) && v.startsWith("/") && !v.startsWith("//");
    const pages = Array.isArray(data?.pages) ? data.pages : [];
    return pages
      .filter((p) => isText(p?.section) && isText(p?.title) && sameSite(p?.url))
      .map((p) => ({
        section: p.section,
        title: p.title,
        url: p.url,
        headings: (Array.isArray(p.headings) ? p.headings : []).filter(
          (h) => isText(h?.text) && sameSite(h?.url),
        ),
      }));
  }

  /**
   * Adds a box above the sidebar groups that finds pages as you type.
   * Once the docs version's search index has loaded (on first focus), it
   * lists every page whose title or headings hold each typed word, as
   * "Group / Page" with the matching headings under it, including pages
   * in collapsed groups and the Web API's route groups. Until then, or if
   * the index cannot be read, it hides sidebar pages whose title does not
   * match. "/" focuses it, Escape clears it.
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
    input.setAttribute("aria-label", "Filter pages and their sections");
    input.autocomplete = "off";
    const hint = document.createElement("kbd");
    hint.textContent = "/";
    hint.setAttribute("aria-hidden", "true");
    box.append(input, hint);
    const results = document.createElement("ul");
    results.className = "nav-results";
    results.setAttribute("aria-label", "Matching pages");
    results.hidden = true;
    const empty = document.createElement("p");
    empty.className = "nav-empty";
    empty.setAttribute("role", "status");
    empty.textContent = "No page matches.";
    empty.hidden = true;
    groups[0].before(box, results);
    groups[groups.length - 1].after(empty);

    /** @type {null | "loading" | "failed" | Awaited<ReturnType<typeof loadSearchIndex>>} */
    let index = null;
    const indexUrl = nav.getAttribute("data-search-index");
    const load = () => {
      if (index !== null || !indexUrl) return;
      index = "loading";
      loadSearchIndex(indexUrl).then(
        (pages) => {
          index = pages;
          apply();
        },
        () => {
          index = "failed";
        },
      );
    };

    /**
     * Lists the index's pages that match every word.
     *
     * @param {string[]} words - The lowercased words typed.
     * @returns {number} How many pages are listed.
     */
    const search = (words) => {
      if (!Array.isArray(index)) return 0;
      const has = (text) => {
        const lower = text.toLowerCase();
        return words.every((w) => lower.includes(w));
      };
      const titled = [];
      const headed = [];
      for (const page of index) {
        const hits = page.headings.filter((h) => has(h.text)).slice(0, MAX_HEADINGS);
        if (has(`${page.section} ${page.title}`)) titled.push({ page, hits });
        else if (hits.length > 0) headed.push({ page, hits });
      }
      const here = window.location.pathname;
      const listed = [...titled, ...headed].slice(0, MAX_RESULTS).map(({ page, hits }) => {
        const item = document.createElement("li");
        const link = document.createElement("a");
        link.href = page.url;
        if (page.url === here) link.setAttribute("aria-current", "page");
        const section = document.createElement("span");
        section.className = "nav-result-section";
        section.textContent = `${page.section} / `;
        link.append(section, page.title);
        item.append(link);
        if (hits.length > 0) {
          const sub = document.createElement("ul");
          sub.className = "nav-sub";
          for (const hit of hits) {
            const li = document.createElement("li");
            const a = document.createElement("a");
            a.href = hit.url;
            // A route shows its method as a badge, as in the "On this page" rail.
            const route = /^(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS) (\/\S*)$/.exec(hit.text);
            if (route) {
              const badge = document.createElement("span");
              badge.className = `api-method api-method-${route[1].toLowerCase()}`;
              badge.textContent = route[1];
              // Long paths wrap only after a slash.
              const path = document.createElement("code");
              route[2].split(/(?<=\/)/).forEach((part, i) => {
                if (i > 0) path.append(document.createElement("wbr"));
                path.append(part);
              });
              a.classList.add("nav-result-route");
              a.append(badge, " ", path);
            } else {
              a.textContent = hit.text;
            }
            li.append(a);
            sub.append(li);
          }
          item.append(sub);
        }
        return item;
      });
      results.replaceChildren(...listed);
      return listed.length;
    };

    const apply = () => {
      const query = input.value.trim().toLowerCase();
      const indexed = query !== "" && Array.isArray(index);
      let shown = 0;
      for (const group of groups) {
        let groupShown = 0;
        for (const item of group.querySelectorAll("li")) {
          const match = query === "" || item.textContent.toLowerCase().includes(query);
          item.hidden = !match;
          if (match) groupShown++;
        }
        group.hidden = indexed || groupShown === 0;
        group.open = query === "" ? (wasOpen.get(group) ?? false) : groupShown > 0;
        shown += groupShown;
      }
      results.hidden = !indexed;
      if (indexed) shown = search(query.split(/\s+/));
      else results.replaceChildren();
      empty.hidden = shown > 0;
    };
    input.addEventListener("focus", load);
    input.addEventListener("input", () => {
      load();
      apply();
    });
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

  /**
   * Makes the phone menus behave like menus: opening one closes the other
   * (also done by their shared `name` where browsers support it), and a tap
   * outside, Escape or following a link closes the open one.
   */
  function dismissMenus() {
    const menus = [...document.querySelectorAll(".docs-menus details.menu")];
    if (menus.length === 0) return;
    for (const menu of menus) {
      menu.addEventListener("toggle", () => {
        if (!menu.open) return;
        for (const other of menus) if (other !== menu) other.open = false;
      });
      // A link in the menu, such as an "On this page" anchor, has done its job.
      menu.querySelector(".menu-body")?.addEventListener("click", (event) => {
        if (event.target instanceof Element && event.target.closest("a")) menu.open = false;
      });
    }
    document.addEventListener("click", (event) => {
      for (const menu of menus)
        if (menu.open && !(event.target instanceof Node && menu.contains(event.target)))
          menu.open = false;
    });
    document.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      const open = menus.find((menu) => menu.open);
      if (!open) return;
      open.open = false;
      open.querySelector("summary")?.focus();
    });
  }

  /**
   * Turns a card's panels into tabs: a row of buttons above the panels, one
   * panel shown at a time. Arrow keys, Home and End move between the tabs,
   * as the ARIA tabs pattern describes.
   *
   * @param {Element} card - The card, which gets the `has-tabs` class.
   * @param {Element[]} panels - Its panels, each with an id.
   * @param {{ list: string, group: string, tab: (panel: Element, i: number) => HTMLButtonElement }} parts -
   *   The class of the tab list and of the group that holds list and
   *   panels, and a function that makes the button for a panel.
   * @param {(index: number) => void} [chosen] - Called when a tab is picked.
   * @returns {(index: number) => void} Selects the tab at an index.
   */
  function tabCard(card, panels, parts, chosen) {
    const list = document.createElement("div");
    list.className = parts.list;
    list.setAttribute("role", "tablist");
    const heading = card.querySelector("h3");
    if (heading && heading.id) list.setAttribute("aria-labelledby", heading.id);
    const tabs = panels.map((panel, i) => {
      const tab = parts.tab(panel, i);
      tab.type = "button";
      tab.id = `${panel.id}-tab`;
      tab.setAttribute("role", "tab");
      tab.setAttribute("aria-controls", panel.id);
      panel.setAttribute("role", "tabpanel");
      panel.setAttribute("aria-labelledby", tab.id);
      panel.tabIndex = 0;
      return tab;
    });
    const select = (index, focus) => {
      tabs.forEach((tab, i) => {
        const on = i === index;
        tab.setAttribute("aria-selected", String(on));
        tab.tabIndex = on ? 0 : -1;
        panels[i].hidden = !on;
      });
      if (focus) tabs[index].focus();
    };
    const pick = (index, focus) => {
      select(index, focus);
      if (chosen) chosen(index);
    };
    tabs.forEach((tab, i) => {
      tab.addEventListener("click", () => pick(i, false));
      tab.addEventListener("keydown", (event) => {
        const last = tabs.length - 1;
        const to = {
          ArrowRight: i === last ? 0 : i + 1,
          ArrowLeft: i === 0 ? last : i - 1,
          Home: 0,
          End: last,
        }[event.key];
        if (to === undefined) return;
        event.preventDefault();
        pick(to, true);
      });
    });
    list.append(...tabs);
    const group = document.createElement("div");
    group.className = parts.group;
    panels[0].before(group);
    group.append(list, ...panels);
    card.classList.add("has-tabs");
    return (index) => select(index, false);
  }

  /**
   * A display preference kept in the browser's local storage, which the
   * privacy page lists. A browser that refuses storage keeps the choice for
   * the page only.
   *
   * @param {string} key - The storage key.
   * @returns {{ get: () => string, set: (value: string) => void }} Reads
   *   the kept value ("" when there is none) and keeps a new one.
   */
  function preference(key) {
    return {
      get: () => {
        try {
          return localStorage.getItem(key) || "";
        } catch {
          return "";
        }
      },
      set: (value) => {
        try {
          localStorage.setItem(key, value);
        } catch {
          // Storage is off or full; the choice then lasts for this page.
        }
      },
    };
  }

  /**
   * Applies a kept preference now and again when the page comes back from
   * the back/forward cache, where a choice made on another page since would
   * otherwise not show.
   *
   * @param {{ get: () => string }} kept - The preference.
   * @param {(value: string) => void} show - Applies a value.
   */
  function follow(kept, show) {
    show(kept.get());
    window.addEventListener("pageshow", (event) => {
      if (event.persisted) show(kept.get());
    });
  }

  /** The browser storage key of the response view last picked. */
  const VIEW_KEY = "docs.response-view";

  /**
   * Turns each example response card into tabs, one per status, with an
   * Example/Schema switch: Example shows the status's example body, Schema
   * the fields of its body, copied from the route's Responses list.
   * Picking a view in one card picks it in every card on the page, and the
   * choice is kept in the browser for the next page. Without this, the
   * panels show one after another and the schema sits in the Responses
   * list only.
   */
  function addResponseTabs() {
    const cards = [];
    for (const card of document.querySelectorAll(".api-responses")) {
      const panels = [...card.querySelectorAll(":scope > .api-response")];
      if (panels.length === 0) continue;
      const select = tabCard(card, panels, {
        list: "api-response-tabs",
        group: "api-response-group",
        tab: (panel, i) => {
          const tab = document.createElement("button");
          const mark = panel.querySelector(".api-response-status .api-status");
          tab.className = mark ? mark.className : "api-status";
          tab.textContent = panel.dataset.status || String(i + 1);
          return tab;
        },
      });
      select(0);
      const toggle = addResponseViews(card, panels, (view) => choose(view));
      if (toggle) cards.push(toggle);
    }
    const show = (view) => {
      for (const card of cards) card(view === "schema" ? "schema" : "example");
    };
    const kept = preference(VIEW_KEY);
    const choose = (view) => {
      show(view);
      kept.set(view);
    };
    follow(kept, show);
  }

  /**
   * Adds the Example/Schema switch to a response card whose statuses all
   * have an item in the route's Responses list, and gives each panel a
   * schema view copied from that item (its description, body and fields).
   *
   * @param {Element} card - The card, already laid out as tabs.
   * @param {Element[]} panels - Its status panels.
   * @param {(view: string) => void} pick - Called with the view a button picks.
   * @returns {((view: string) => void) | null} Shows a view ("example" or
   *   "schema"), or null when a status has no item.
   */
  function addResponseViews(card, panels, pick) {
    const section = card.closest(".api-op");
    const items = section ? [...section.querySelectorAll("li.api-response-item")] : [];
    const itemOf = (status) =>
      items.find(
        (item) => item.querySelector(".api-prop-head .api-status")?.textContent === status,
      );
    const list = card.querySelector(".api-response-tabs");
    if (!list || panels.some((panel) => !itemOf(panel.dataset.status || ""))) return null;
    for (const panel of panels) {
      const example = document.createElement("div");
      example.className = "api-response-example";
      example.append(...panel.childNodes);
      const schema = document.createElement("div");
      schema.className = "api-response-schema";
      for (const node of itemOf(panel.dataset.status || "").childNodes) {
        const copy = node.cloneNode(true);
        if (copy instanceof Element && copy.classList.contains("api-prop-head")) continue;
        schema.append(copy);
      }
      panel.append(example, schema);
    }
    const head = document.createElement("div");
    head.className = "api-response-head";
    list.before(head);
    const toggle = document.createElement("div");
    toggle.className = "api-view-toggle";
    toggle.setAttribute("role", "group");
    toggle.setAttribute("aria-label", "Response view");
    const buttons = ["example", "schema"].map((view) => {
      const button = document.createElement("button");
      button.type = "button";
      button.dataset.view = view;
      button.textContent = view === "schema" ? "Schema" : "Example";
      button.addEventListener("click", () => pick(view));
      return button;
    });
    toggle.append(...buttons);
    head.append(list, toggle);
    const show = (view) => {
      for (const button of buttons)
        button.setAttribute("aria-pressed", String(button.dataset.view === view));
      card.classList.toggle("shows-schema", view === "schema");
    };
    return show;
  }

  /** The browser storage key of the request sample language last picked. */
  const LANGUAGE_KEY = "docs.request-language";

  /**
   * Turns each example request card with more than one language into a
   * language picker, one tab per language. Picking a language in one card
   * picks it in every card on the page, and the choice is kept in the
   * browser for the next page. Without this, every sample shows, one after
   * another.
   */
  function addRequestTabs() {
    const cards = [];
    for (const card of document.querySelectorAll(".api-requests")) {
      const panels = [...card.querySelectorAll(":scope > .api-request")];
      if (panels.length < 2) continue;
      const langs = panels.map((panel) => panel.dataset.lang || "");
      const select = tabCard(
        card,
        panels,
        {
          list: "api-request-tabs",
          group: "api-request-group",
          tab: (panel) => {
            const tab = document.createElement("button");
            tab.className = "api-lang";
            tab.textContent = panel.dataset.label || panel.dataset.lang || "";
            return tab;
          },
        },
        (index) => choose(langs[index]),
      );
      cards.push({ langs, select });
    }
    const show = (lang) => {
      for (const card of cards) {
        const index = card.langs.indexOf(lang);
        card.select(index < 0 ? 0 : index);
      }
    };
    const kept = preference(LANGUAGE_KEY);
    const choose = (lang) => {
      show(lang);
      kept.set(lang);
    };
    follow(kept, show);
  }

  addCopyButtons();
  addResponseTabs();
  addRequestTabs();
  dismissMenus();
  copyHeadingLinks();
  addCopyMarkdown();
  drawTocRail();
  trackSection();
  addSidebarFilter();
})();
