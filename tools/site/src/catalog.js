// The stack drops in once. Its repeatable shuffle is an optional interaction
// confined to the cards; ordinary page scrolling is never repositioned.
(() => {
  "use strict";

  /** Decorative artwork and its fixed-proportion sheet container. */
  const stage = document.querySelector("[data-catalog]");
  const stack = stage?.querySelector(".catalog-stack");
  /** Inert, already escaped sheets rendered from the same JSON as the fallback. */
  const pool = stage?.querySelector("[data-catalog-pool]")?.content;
  if (stack && pool) {
    const groups = [...pool.querySelectorAll("[data-catalog-group]")]
      .map((group) => [...group.querySelectorAll(".catalog-card")])
      .filter((books) => books.length > 0);
    if (groups.length >= 4) {
      // Equal group weights prevent long series from dominating the sample.
      for (let i = groups.length - 1; i > 0; i -= 1) {
        const j = Math.floor(Math.random() * (i + 1));
        [groups[i], groups[j]] = [groups[j], groups[i]];
      }
      stack.replaceChildren(
        ...groups.slice(0, 4).map((books) => {
          const chosen = books[Math.floor(Math.random() * books.length)];
          return chosen.cloneNode(true);
        }),
      );
    }
  }

  /** Current front-to-back order, kept in sync with the rendered DOM. */
  let cards = stack ? [...stack.querySelectorAll(".catalog-card")] : [];
  if (!stack || cards.length < 2) return;
  /** Motion preference and the surfaces occluding the visible viewport. */
  const reduced = matchMedia("(prefers-reduced-motion: reduce)");
  const header = document.querySelector(".site-header");
  const notice = document.querySelector(".ribbon");
  if (typeof CSS !== "undefined" && !CSS.supports("animation-name", "catalog-drop")) return;
  /** Waiting, entering, ready, dragging, shuffling or disabled interaction state. */
  let phase = reduced.matches ? "disabled" : "waiting";
  /** Bounded completion timer; CSS events are never required for recovery. */
  let timer = 0;
  /** Coalesced visibility check while the initial entrance is waiting. */
  let frame = 0;
  /** Last wheel activity; momentum cannot start another shuffle in the same burst. */
  let wheelUntil = 0;
  /** Width at shuffle start, so orientation changes settle it immediately. */
  let shuffleWidth = 0;
  /** Only the current incoming stamp may complete the current shuffle. */
  let incomingStamp = null;
  /** Active pointer, its origin and its fitted displacement in sheet pixels. */
  let drag = null;
  stage.dataset.catalogState = reduced.matches ? "static" : "waiting";

  /**
   * Bounds a rotated sheet around its 50% / 70% transform origin.
   *
   * @param {number} width - Untransformed border-box width in CSS pixels.
   * @param {number} height - Untransformed border-box height in CSS pixels.
   * @param {number} x - Horizontal translation in CSS pixels.
   * @param {number} y - Vertical translation in CSS pixels.
   * @param {number} degrees - Clockwise rotation in degrees.
   * @param {number} scale - Sheet-local scale, before the stack's fit scale.
   * @returns {{left:number, top:number, right:number, bottom:number}} Sheet bounds.
   */
  function sheetBounds(width, height, x, y, degrees, scale = 1) {
    const angle = (degrees * Math.PI) / 180;
    const cosine = Math.cos(angle);
    const sine = Math.sin(angle);
    const points = [
      [0, 0],
      [width, 0],
      [width, height],
      [0, height],
    ].map(([px, py]) => ({
      x: width / 2 + x + ((px - width / 2) * cosine - (py - height * 0.7) * sine) * scale,
      y: height * 0.7 + y + ((px - width / 2) * sine + (py - height * 0.7) * cosine) * scale,
    }));
    return {
      left: Math.min(...points.map((p) => p.x)),
      top: Math.min(...points.map((p) => p.y)),
      right: Math.max(...points.map((p) => p.x)),
      bottom: Math.max(...points.map((p) => p.y)),
    };
  }

  /**
   * Measures the full resting stack independently of the current animation.
   * Also fits unusually short screens without changing document flow.
   *
   * @returns {object|null} Geometry, or null when a complete readable view cannot fit.
   */
  function layout() {
    const viewport = window.visualViewport;
    if (viewport && Math.abs(viewport.scale - 1) > 0.01) return null;
    const viewTop = viewport?.offsetTop || 0;
    const viewLeft = viewport?.offsetLeft || 0;
    const viewHeight = viewport?.height || window.innerHeight;
    const viewWidth = Math.min(
      viewport?.width || window.innerWidth,
      document.documentElement.clientWidth,
    );
    const headerHeight = header?.getBoundingClientRect().height || 0;
    const noticeHeight = notice?.getBoundingClientRect().height || 0;
    const stickyInset = header ? parseFloat(getComputedStyle(header).top) || 0 : 0;
    const gutter = 16;
    const availableHeight = viewHeight - headerHeight - stickyInset - 2 * gutter;
    const hero = stage.closest?.(".hero")?.getBoundingClientRect();
    const safeLeft = Math.max(viewLeft + gutter, (hero?.left ?? viewLeft) + gutter);
    const safeRight = Math.min(
      viewLeft + viewWidth - gutter,
      (hero?.right ?? viewLeft + viewWidth) - gutter,
    );
    const availableWidth = safeRight - safeLeft;
    const width = stack.offsetWidth;
    const height = stack.offsetHeight;
    if (!width || !height) return null;
    const sheets = cards.map((card) => {
      const style = getComputedStyle(card);
      return sheetBounds(
        width,
        height,
        parseFloat(style.getPropertyValue("--card-x")) || 0,
        parseFloat(style.getPropertyValue("--card-y")) || 0,
        parseFloat(style.getPropertyValue("--card-angle")) || 0,
      );
    });
    const bounds = {
      left: Math.min(...sheets.map((b) => b.left)),
      top: Math.min(...sheets.map((b) => b.top)),
      right: Math.max(...sheets.map((b) => b.right)),
      bottom: Math.max(...sheets.map((b) => b.bottom)),
    };
    const fullWidth = bounds.right - bounds.left;
    const fullHeight = bounds.bottom - bounds.top;
    // Static short-screen views still need fitting and centering. Keep those
    // cards readable while allowing ordinary scrolling through the artwork.
    const capturable = availableHeight >= 240 && (availableHeight - 24) / fullHeight >= 0.75;
    const scale = Math.min(
      1,
      Math.max(280, availableHeight - 24) / fullHeight,
      availableWidth / fullWidth,
    );
    stage.style.setProperty("--catalog-height", `${fullHeight * scale + 16}px`);
    const region = stage.getBoundingClientRect();
    const left = Math.max(
      safeLeft,
      Math.min(region.left + (region.width - fullWidth * scale) / 2, safeRight - fullWidth * scale),
    );
    const offsetX = left - region.left - stack.offsetLeft - bounds.left * scale;
    const offsetY = (region.height - fullHeight * scale) / 2 - stack.offsetTop - bounds.top * scale;
    stack.style.setProperty("--catalog-scale", String(scale));
    stack.style.setProperty("--catalog-offset-x", `${offsetX}px`);
    stack.style.setProperty("--catalog-offset-y", `${offsetY}px`);
    const originY = region.top + window.scrollY + stack.offsetTop + offsetY;
    const top = originY + bounds.top * scale;
    const bottom = originY + bounds.bottom * scale;
    return {
      width,
      height,
      scale,
      capturable,
      originY,
      top,
      bottom,
      originX: left - bounds.left * scale,
      safeLeft,
      safeRight,
      safeBottom: viewTop + viewHeight - gutter,
      safeTop: (scrollY) =>
        viewTop + Math.max(stickyInset, noticeHeight - scrollY) + headerHeight + gutter,
    };
  }

  /**
   * Tests the complete resting view against the sticky header and viewport.
   *
   * @param {object} geometry - The current fitted geometry.
   * @param {number} scrollY - Candidate page position in CSS pixels.
   * @returns {boolean} Whether every resting sheet fits with its safety margin.
   */
  function fits(geometry, scrollY) {
    return (
      geometry.capturable &&
      geometry.top - scrollY >= geometry.safeTop(scrollY) - 1 &&
      geometry.bottom - scrollY <= geometry.safeBottom + 1
    );
  }

  /**
   * Reduces the lift only when its full path would pass under the header or
   * outside the viewport. Resting cards retain their approved appearance.
   *
   * @param {object} geometry - Fitted stack bounds.
   * @param {number} scrollY - Position at which the shuffle will run.
   * @param {number} direction - Horizontal direction, either -1 or 1.
   * @param {number} side - Desired horizontal lift in unscaled sheet pixels.
   * @param {number} bleed - User-drag allowance past a horizontal viewport edge, in pixels.
   */
  function fitLift(geometry, scrollY, direction = 1, side = 46, bleed = 0) {
    let low = 0;
    let high = 1;
    for (let i = 0; i < 12; i += 1) {
      const fraction = (low + high) / 2;
      const peak = sheetBounds(
        geometry.width,
        geometry.height,
        side * direction * fraction,
        -95 * fraction,
        9 * direction * fraction,
        1 + 0.035 * fraction,
      );
      const top = geometry.originY - scrollY + peak.top * geometry.scale;
      const bottom = geometry.originY - scrollY + peak.bottom * geometry.scale;
      const left = geometry.originX + peak.left * geometry.scale;
      const right = geometry.originX + peak.right * geometry.scale;
      if (
        top >= geometry.safeTop(scrollY) &&
        bottom <= geometry.safeBottom &&
        left >= geometry.safeLeft - bleed &&
        right <= geometry.safeRight + bleed
      )
        low = fraction;
      else high = fraction;
    }
    stack.style.setProperty("--catalog-lift", `${95 * low}px`);
    stack.style.setProperty("--catalog-side", `${side * direction * low}px`);
    stack.style.setProperty("--catalog-turn", `${9 * direction * low}deg`);
    stack.style.setProperty("--catalog-lift-scale", String(1 + 0.035 * low));
  }

  /**
   * Writes the constrained drop distance below the sticky header.
   *
   * @param {object} geometry - Current fitted resting bounds.
   */
  function fitEntrance(geometry) {
    const distance = Math.max(
      0,
      (geometry.top - window.scrollY - geometry.safeTop(window.scrollY)) / geometry.scale,
    );
    stack.style.setProperty("--catalog-drop-distance", `${Math.min(95, distance)}px`);
  }

  /** Clears displacement on the current front card. */
  function clearDragPose() {
    cards[0].classList.remove("is-dragging");
    for (const name of ["--drag-x", "--drag-y", "--drag-turn"]) cards[0].style.removeProperty(name);
  }

  /** Releases pointer capture without committing a shuffle. */
  function cancelDrag() {
    const pointer = drag;
    drag = null;
    if (pointer && cards[0].hasPointerCapture?.(pointer.id))
      cards[0].releasePointerCapture(pointer.id);
    clearDragPose();
    if (phase === "dragging") {
      phase = "ready";
      stage.dataset.catalogState = "ready";
    }
  }

  /** Commits the completed turn and restores the idle front-to-back styling. */
  function completeShuffle() {
    if (phase !== "shuffling") return;
    window.clearTimeout(timer);
    const outgoing = cards[0];
    stack.append(outgoing);
    cards = [...cards.slice(1), outgoing];
    for (const card of cards) {
      card.classList.remove("is-outgoing", "is-incoming", "is-dragging");
      for (const name of [
        "--target-x",
        "--target-y",
        "--target-angle",
        "--target-z",
        "--drag-x",
        "--drag-y",
        "--drag-turn",
      ])
        card.style.removeProperty(name);
    }
    incomingStamp = null;
    phase = "ready";
    stage.dataset.catalogState = "ready";
  }

  /**
   * Sends the front card behind the stack and stamps the newly revealed record.
   *
   * @param {object} geometry - Fully visible fitted stack bounds.
   */
  function shuffle(geometry) {
    window.clearTimeout(timer);
    const displacement = parseFloat(cards[0].style.getPropertyValue("--drag-x")) || 0;
    fitLift(
      geometry,
      window.scrollY,
      displacement < 0 ? -1 : 1,
      Math.max(46, Math.abs(displacement) + 40),
      displacement ? 64 : 0,
    );
    shuffleWidth = document.documentElement.clientWidth;
    cards.forEach((card, index) => {
      const destination = (index + cards.length - 1) % cards.length;
      card.style.setProperty("--target-x", `${destination * 13}px`);
      card.style.setProperty("--target-y", `${destination * 14}px`);
      card.style.setProperty("--target-angle", `${destination * 2.4}deg`);
      card.style.setProperty("--target-z", String(cards.length - destination));
    });
    cards[0].classList.remove("is-dragging");
    cards[0].classList.add("is-outgoing");
    cards[1].classList.add("is-incoming");
    incomingStamp = cards[1].querySelector(".stamp-die");
    phase = "shuffling";
    stage.dataset.catalogState = "shuffling";
    timer = window.setTimeout(completeShuffle, 1250);
  }

  /** Starts the one-time entrance only after the complete stack fits. */
  function reveal() {
    const geometry = layout();
    if (phase !== "waiting") return;
    stage.dataset.catalogState = geometry?.capturable ? "waiting" : "static";
    if (!geometry || !fits(geometry, window.scrollY)) return;
    fitEntrance(geometry);
    phase = "entering";
    stage.dataset.catalogState = "entering";
    window.removeEventListener("scroll", onScroll);
    timer = window.setTimeout(() => {
      if (phase !== "entering") return;
      phase = "ready";
      stage.dataset.catalogState = "ready";
    }, 1550);
  }

  /**
   * Converts a wheel gesture over the actual sheets into one optional shuffle.
   *
   * @param {WheelEvent} event - Zoom, sideways and noncancelable input stay native.
   */
  function onWheel(event) {
    if (
      reduced.matches ||
      event.ctrlKey ||
      event.shiftKey ||
      !event.cancelable ||
      !event.deltaY ||
      Math.abs(event.deltaX) > Math.abs(event.deltaY) ||
      !(event.target instanceof Element) ||
      !event.target.closest(".catalog-card")
    )
      return;
    const fresh = performance.now() >= wheelUntil;
    wheelUntil = performance.now() + 300;
    if (phase === "shuffling") {
      event.preventDefault();
      return;
    }
    if (phase !== "ready" || !fresh || drag) return;
    const geometry = layout();
    if (!geometry || !fits(geometry, window.scrollY)) return;
    shuffle(geometry);
    event.preventDefault();
  }

  /**
   * Captures only the front sheet; taps and native vertical scrolling remain available.
   *
   * @param {PointerEvent} event - Mouse, touch or pen starting over the top record.
   */
  function onPointerDown(event) {
    if (!event.isPrimary) {
      cancelDrag();
      return;
    }
    if (
      phase !== "ready" ||
      reduced.matches ||
      event.button !== 0 ||
      !(event.target instanceof Element) ||
      event.target.closest(".catalog-card") !== cards[0]
    )
      return;
    const geometry = layout();
    if (!geometry || !fits(geometry, window.scrollY)) return;
    drag = {
      id: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      active: false,
      type: event.pointerType,
    };
    cards[0].setPointerCapture(event.pointerId);
    if (event.pointerType === "mouse") event.preventDefault();
  }

  /**
   * Moves the front sheet with the pointer, constrained to the visible viewport.
   *
   * @param {PointerEvent} event - The captured pointer's current client coordinates.
   */
  function onPointerMove(event) {
    if (!drag || event.pointerId !== drag.id) return;
    const dx = event.clientX - drag.x;
    const dy = event.clientY - drag.y;
    if (!drag.active) {
      if (Math.hypot(dx, dy) < 12) return;
      // Touch starts horizontally; the browser owns vertical pan and pinch.
      if (drag.type !== "mouse" && Math.abs(dy) > Math.abs(dx)) return;
      drag.active = true;
      phase = "dragging";
      stage.dataset.catalogState = "dragging";
      cards[0].classList.add("is-dragging");
    }
    const geometry = layout();
    if (!geometry || !fits(geometry, window.scrollY)) {
      cancelDrag();
      return;
    }
    const x = Math.max(-180, Math.min(180, dx / geometry.scale));
    const y = Math.max(-110, Math.min(110, dy / geometry.scale));
    const turn = Math.max(-8, Math.min(8, x * 0.04));
    let low = 0;
    let high = 1;
    for (let i = 0; i < 12; i += 1) {
      const fraction = (low + high) / 2;
      const bounds = sheetBounds(
        geometry.width,
        geometry.height,
        x * fraction,
        y * fraction,
        turn * fraction,
      );
      if (
        geometry.originX + bounds.left * geometry.scale >= geometry.safeLeft - 64 &&
        geometry.originX + bounds.right * geometry.scale <= geometry.safeRight + 64 &&
        geometry.originY - window.scrollY + bounds.top * geometry.scale >=
          geometry.safeTop(window.scrollY) &&
        geometry.originY - window.scrollY + bounds.bottom * geometry.scale <= geometry.safeBottom
      )
        low = fraction;
      else high = fraction;
    }
    cards[0].style.setProperty("--drag-x", `${x * low}px`);
    cards[0].style.setProperty("--drag-y", `${y * low}px`);
    cards[0].style.setProperty("--drag-turn", `${turn * low}deg`);
    if (event.cancelable) event.preventDefault();
  }

  /**
   * Commits a deliberate drag, or releases a tap without changing records.
   *
   * @param {PointerEvent} event - The captured pointer being released.
   */
  function onPointerUp(event) {
    if (!drag || event.pointerId !== drag.id) return;
    const committed = drag.active;
    const id = drag.id;
    drag = null;
    if (cards[0].hasPointerCapture(id)) cards[0].releasePointerCapture(id);
    const geometry = layout();
    if (committed && geometry && fits(geometry, window.scrollY)) shuffle(geometry);
    else {
      clearDragPose();
      phase = "ready";
      stage.dataset.catalogState = "ready";
    }
  }

  /**
   * Cancels native pan/zoom takeovers, including lost pointer capture.
   *
   * @param {PointerEvent} event - Pointer whose ownership was lost.
   */
  function onPointerCancel(event) {
    if (drag?.id === event.pointerId) cancelDrag();
  }

  /** Coalesces initial visibility measurements without canceling native scrolling. */
  function onScroll() {
    if (frame || phase !== "waiting") return;
    frame = window.requestAnimationFrame(() => {
      frame = 0;
      reveal();
    });
  }

  /** Refits responsive artwork; resizing never moves or pins the page. */
  function resize() {
    const geometry = layout();
    if (drag) cancelDrag();
    if (
      phase === "shuffling" &&
      (!geometry ||
        shuffleWidth !== document.documentElement.clientWidth ||
        !fits(geometry, window.scrollY))
    )
      completeShuffle();
    if (phase === "waiting") reveal();
  }

  /** Settles animations and pointer ownership when leaving the page. */
  function onPageHide() {
    cancelDrag();
    completeShuffle();
    window.clearTimeout(timer);
    if (frame) window.cancelAnimationFrame(frame);
    frame = 0;
    if (phase === "entering") {
      phase = "ready";
      stage.dataset.catalogState = "ready";
    }
  }

  /** Disables optional interaction immediately when reduced motion is requested. */
  function disableMotion() {
    onPageHide();
    phase = "disabled";
    stage.dataset.catalogState = "static";
    stack.removeEventListener("wheel", onWheel);
    stack.removeEventListener("pointerdown", onPointerDown);
    stack.removeEventListener("pointermove", onPointerMove);
    stack.removeEventListener("pointerup", onPointerUp);
    stack.removeEventListener("pointercancel", onPointerCancel);
    stack.removeEventListener("lostpointercapture", onPointerCancel);
    window.removeEventListener("scroll", onScroll);
  }

  if (!reduced.matches) {
    stack.addEventListener("wheel", onWheel, { passive: false });
    stack.addEventListener("pointerdown", onPointerDown);
    stack.addEventListener("pointermove", onPointerMove);
    stack.addEventListener("pointerup", onPointerUp);
    stack.addEventListener("pointercancel", onPointerCancel);
    stack.addEventListener("lostpointercapture", onPointerCancel);
    window.addEventListener("scroll", onScroll, { passive: true });
  }
  window.addEventListener("keydown", () => {
    cancelDrag();
    completeShuffle();
  });
  window.addEventListener("pagehide", onPageHide);
  window.addEventListener("resize", resize, { passive: true });
  window.visualViewport?.addEventListener("resize", resize, { passive: true });
  reduced.addEventListener("change", () => {
    if (reduced.matches) disableMotion();
  });
  stage.addEventListener("animationend", (event) => {
    if (
      phase === "shuffling" &&
      event.animationName === "catalog-stamp" &&
      event.target === incomingStamp
    )
      completeShuffle();
  });
  if (typeof ResizeObserver === "function") {
    const observer = new ResizeObserver(resize);
    observer.observe(stage);
    if (header) observer.observe(header);
    if (notice) observer.observe(notice);
  }
  if (document.fonts) document.fonts.ready.then(resize);
  resize();
})();
