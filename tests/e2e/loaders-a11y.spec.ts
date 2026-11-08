/** E2E: loading states + reduced-motion. PageLoader (components/ui/loader.tsx): role="status",
 * brand svg, randomized humor line; index.css `.ld-*` animations are disabled under
 * prefers-reduced-motion. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

const LD_CLASSES = ".ld-arc-rock, .ld-dot-pulse, .ld-flip, .ld-ring, .ld-dot-bounce, .ld-shadow, .ld-comet";

test("PageLoader shows role=status + humor line while throttled, then disappears", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.route("**/api/dashboards**", async (route) => {
    await new Promise((r) => setTimeout(r, 2000));
    await route.continue();
  });

  await page.goto("/dashboards");

  const status = page.getByRole("status");
  await expect(status).toBeVisible({ timeout: 3000 });
  const humorLine = status.locator("p");
  await expect(humorLine).toHaveText(/\w/, { timeout: 2000 });

  await expect(status).not.toBeVisible({ timeout: 8000 });

  await ctx.close();
});

test("reduced motion → loader still renders but computed animation is none", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await page.emulateMedia({ reducedMotion: "reduce" });

  await page.route("**/api/dashboards**", async (route) => {
    await new Promise((r) => setTimeout(r, 2000));
    await route.continue();
  });

  await page.goto("/dashboards");
  await expect(page.getByRole("status")).toBeVisible({ timeout: 3000 });

  const ldEl = page.locator(LD_CLASSES).first();
  await expect(ldEl).toBeVisible({ timeout: 2000 });
  // Poll: the first post-mount paint can report an empty computed value before style recalc
  // settles.
  await expect
    .poll(() => ldEl.evaluate((el) => getComputedStyle(el).animationName), { timeout: 2000 })
    .toBe("none");

  await ctx.close();
});

test("mutation button pending state: slow-routed Execute shows disabled button + spinner", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-loader-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-loader-q-${Date.now()}`,
    query: "SELECT 1 AS answer",
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto(`/queries/${q.id}`);

    // Execute runs ad-hoc (POST /api/query_results), not the saved-query endpoint.
    await page.route("**/api/query_results", async (route) => {
      if (route.request().method() === "POST") {
        await new Promise((r) => setTimeout(r, 2000));
      }
      await route.continue();
    });

    // Non-anchored: the accessible name flips between "Execute" and "Running…" while pending.
    const runBtn = page.getByRole("button", { name: /Execute|Running/ });
    await runBtn.click();

    await expect(runBtn).toContainText("Running…");
    await expect(runBtn).toBeDisabled();
    // The button spinner is decorative (aria-hidden, no status role); the button text is the
    // announcement.
    await expect(runBtn.locator("svg")).toBeVisible();

    await expect(runBtn).toContainText("Execute", { timeout: 10_000 });
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
