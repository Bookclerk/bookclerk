// Browser-level regression checks for the optional catalog interaction.
// Trusted wheel and touch input exercise native hit testing and pan arbitration.
import assert from "node:assert/strict";
import type { Browser, Page } from "playwright-core";

/** Waits for the decorative stack to finish its current bounded animation. */
async function ready(page: Page): Promise<void> {
  await page.waitForFunction(
    () =>
      document.querySelector<HTMLElement>("[data-catalog]")?.dataset["catalogState"] === "ready",
    undefined,
    { timeout: 5000 },
  );
}

/** Reads the current top record from the actual DOM order. */
async function front(page: Page): Promise<string> {
  return page.locator(".catalog-card .card-title strong").first().innerText();
}

/**
 * Exercises trusted browser input without touching any external service.
 *
 * @param browser - The audit's Chromium instance.
 * @param origin - The locally served site origin.
 * @returns Empty on success, or one actionable regression description.
 */
export async function auditCatalog(browser: Browser, origin: string): Promise<string[]> {
  const context = await browser.newContext({
    viewport: { width: 1280, height: 800 },
    reducedMotion: "no-preference",
  });
  context.setDefaultTimeout(5000);
  try {
    const page = await context.newPage();
    await page.goto(origin, { waitUntil: "load" });
    await ready(page);
    const groups = await page
      .locator(".catalog-stack .catalog-card")
      .evaluateAll((cards) => cards.map((card) => (card as HTMLElement).dataset["group"]));
    assert.equal(groups.length, 4);
    assert.equal(new Set(groups).size, 4, "A page must not repeat a series or shared universe");
    const first = await front(page);
    // A wheel over the intro remains normal page scrolling.
    await page.mouse.move(100, 400);
    await page.mouse.wheel(0, 100);
    await page.waitForFunction(() => window.scrollY > 20);
    assert.equal(await front(page), first);
    assert.equal(await page.locator("[data-catalog]").getAttribute("data-catalog-state"), "ready");
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.waitForFunction(() => window.scrollY === 0);
    const box = await page.locator(".catalog-card").first().boundingBox();
    assert.ok(box);
    const next = await page.locator(".catalog-card .card-title strong").nth(1).innerText();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.wheel(0, 100);
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLElement>("[data-catalog]")?.dataset["catalogState"] ===
        "shuffling",
    );
    assert.equal(
      await page
        .locator(".is-incoming .stamp-die")
        .evaluate((element) => getComputedStyle(element).animationName),
      "catalog-stamp",
    );
    await ready(page);
    assert.equal(await front(page), next);
    assert.equal(await page.evaluate(() => window.scrollY), 0);
    assert.equal(await page.locator(".catalog-card .card-title strong").last().innerText(), first);
    const overflow = await page.evaluate(async () => {
      await document.fonts.ready;
      const stack = document.querySelector<HTMLElement>(".catalog-stack");
      const pool = document.querySelector<HTMLTemplateElement>("[data-catalog-pool]");
      if (!stack || !pool) return ["The catalog sampling pool is missing"];
      const original = [...stack.childNodes];
      const failures: string[] = [];
      try {
        for (const sheet of pool.content.querySelectorAll<HTMLElement>(".catalog-card")) {
          const card = sheet.cloneNode(true) as HTMLElement;
          stack.replaceChildren(card);
          const title = card.querySelector("strong")?.textContent ?? "Unknown record";
          const metadata = card.querySelector("dl")?.getBoundingClientRect();
          const stamp = card.querySelector(".stamp:not(.stamp-die)")?.getBoundingClientRect();
          if (!metadata || !stamp || metadata.bottom > stamp.top - 6)
            failures.push(`${title}: metadata overlaps the stamp`);
          for (const field of card.querySelectorAll<HTMLElement>("dd"))
            if (field.scrollWidth > field.clientWidth + 1)
              failures.push(`${title}: field overflows horizontally`);
        }
      } finally {
        stack.replaceChildren(...original);
      }
      return failures;
    });
    assert.deepEqual(overflow, [], overflow.join("; "));
  } catch (error) {
    return [`catalog wheel: ${error instanceof Error ? error.message : String(error)}`];
  } finally {
    await context.close();
  }

  const mobile = await browser.newContext({
    viewport: { width: 390, height: 844 },
    isMobile: true,
    hasTouch: true,
    reducedMotion: "no-preference",
  });
  mobile.setDefaultTimeout(5000);
  let touchStep = "initial entrance";
  try {
    const page = await mobile.newPage();
    await page.goto(origin, { waitUntil: "load" });
    await page
      .locator("[data-catalog]")
      .evaluate((element) => element.scrollIntoView({ block: "center", behavior: "instant" }));
    await ready(page);
    const groups = await page
      .locator(".catalog-stack .catalog-card")
      .evaluateAll((cards) => cards.map((card) => (card as HTMLElement).dataset["group"]));
    assert.equal(groups.length, 4);
    assert.equal(new Set(groups).size, 4, "A page must not repeat a series or shared universe");
    const first = await front(page);
    const next = await page.locator(".catalog-card .card-title strong").nth(1).innerText();
    const box = await page.locator(".catalog-card").first().boundingBox();
    assert.ok(box);
    const x = box.x + box.width * 0.4;
    const y = box.y + box.height * 0.5;
    const scrollY = await page.evaluate(() => window.scrollY);
    const input = await mobile.newCDPSession(page);
    touchStep = "horizontal drag";
    await input.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [{ x, y }] });
    for (const distance of [16, 32, 48, 64, 80])
      await input.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [{ x: x + distance, y }],
      });
    await page.waitForFunction(
      () =>
        document.querySelector<HTMLElement>("[data-catalog]")?.dataset["catalogState"] ===
        "dragging",
    );
    const displacement = await page
      .locator(".catalog-card")
      .first()
      .evaluate((element) =>
        parseFloat((element as HTMLElement).style.getPropertyValue("--drag-x")),
      );
    assert.ok(displacement > 20, `Touch should visibly drag the sheet, got ${displacement}px`);
    touchStep = "release and stamp";
    await input.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await ready(page);
    assert.equal(await front(page), next);
    assert.equal(await page.evaluate(() => window.scrollY), scrollY);
    assert.equal(await page.locator(".catalog-card .card-title strong").last().innerText(), first);

    touchStep = "native vertical swipe";
    // A fresh vertical swipe on the top card is handled by the browser.
    const current = await page.locator(".catalog-card").first().boundingBox();
    assert.ok(current);
    const startX = current.x + current.width / 2;
    const startY = current.y + current.height * 0.75;
    await input.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x: startX, y: startY }],
    });
    for (const distance of [20, 40, 70, 100, 130])
      await input.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [{ x: startX, y: startY - distance }],
      });
    await input.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await page.waitForFunction((before) => window.scrollY > before + 30, scrollY);
    assert.equal(await front(page), next);
    assert.equal(await page.locator(".is-dragging").count(), 0);
    await input.detach();
    return [];
  } catch (error) {
    const diagnostic = await mobile.pages()[0]?.evaluate(() => ({
      width: innerWidth,
      height: innerHeight,
      scrollY,
      state: document.querySelector<HTMLElement>("[data-catalog]")?.dataset["catalogState"],
      art: document.querySelector("[data-catalog]")?.getBoundingClientRect().toJSON(),
      header: document.querySelector("header")?.getBoundingClientRect().toJSON(),
      sheet: document.querySelector(".catalog-card")?.getBoundingClientRect().toJSON(),
    }));
    return [
      `catalog touch (${touchStep}): ${error instanceof Error ? error.message : String(error)}; ${JSON.stringify(diagnostic)}`,
    ];
  } finally {
    await mobile.close();
  }
}
