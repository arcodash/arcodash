import { test, expect } from "@playwright/test";

// index.html preload: /api/session fires before the entry script, and getSession() reuses that
// request.
test("preloads /api/session once, before the entry script finishes loading", async ({ page }) => {
  const sessionRequests: string[] = [];
  page.on("request", (req) => {
    if (new URL(req.url()).pathname === "/api/session") sessionRequests.push(req.url());
  });

  // The preload is cross-origin here, so "started first" is a coin flip; delay only the entry
  // script for a deterministic margin (still fails if the preload waits on the entry chunk).
  await page.route("**/src/main.tsx", async (route) => {
    await new Promise((r) => setTimeout(r, 300));
    await route.continue();
  });

  await page.goto("/login");

  // The session fetch isn't awaited by "load"; wait for its resource-timing entry (responseEnd).
  await page.waitForFunction(() => {
    const entries = performance.getEntriesByType("resource") as PerformanceResourceTiming[];
    return entries.some((e) => {
      try {
        return new URL(e.name).pathname === "/api/session" && e.responseEnd > 0;
      } catch {
        return false;
      }
    });
  });

  // Wait for the app to boot, or a double-fetch on mount could slip past.
  await expect(page.locator("#email")).toBeVisible();
  expect(sessionRequests).toHaveLength(1);

  const timing = await page.evaluate(() => {
    // The entry script is the last module script (dev injects an HMR preamble and /@vite/client
    // first).
    const moduleScripts = document.querySelectorAll<HTMLScriptElement>('script[type="module"][src]');
    const entrySrc = moduleScripts[moduleScripts.length - 1]?.src ?? "";
    const entries = performance.getEntriesByType("resource") as PerformanceResourceTiming[];
    return {
      entryResponseEnd: entries.find((e) => e.name === entrySrc)?.responseEnd,
      sessionStart: entries.find((e) => new URL(e.name).pathname === "/api/session")?.startTime,
    };
  });

  expect(timing.entryResponseEnd).toBeDefined();
  expect(timing.sessionStart).toBeDefined();
  expect(timing.sessionStart).toBeLessThan(timing.entryResponseEnd!);
});
