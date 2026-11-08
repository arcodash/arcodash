// `q` is Postgres full-text search, not substring: a fragment like "zzz-a" never matches, so search
// terms are complete words ("alpha").
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

test("queries search: exactly one debounced request; keepPreviousData avoids blank flash", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-search-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const stamp = Date.now();
  const names = [
    `e2e-dbnc-alpha-${stamp}`,
    `e2e-dbnc-beta-${stamp}`,
    `e2e-dbnc-gamma-${stamp}`,
  ];
  const created = [];
  for (const name of names) {
    created.push(await api.createQuery({ data_source_id: ds.id, name, query: "SELECT 1" }));
  }

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/queries");
    await expect(page.getByText(names[0])).toBeVisible({ timeout: 8000 });

    const search = page.getByPlaceholder("Search queries…");

    const reqUrls: string[] = [];
    const onReq = (req: import("@playwright/test").Request) => {
      const url = req.url();
      if (url.includes("/api/queries?") && url.includes("q=")) reqUrls.push(url);
    };
    page.on("request", onReq);

    await search.pressSequentially("alpha", { delay: 30 }); // whole word, 5 chars
    await page.waitForTimeout(600); // past the 250ms debounce + round trip
    page.off("request", onReq);

    expect(reqUrls.length).toBe(1);
    expect(reqUrls[0]).toContain("q=alpha");
    await expect(page.getByText(names[0])).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(names[1])).not.toBeVisible();

    await search.fill("");
    await page.waitForTimeout(400);
    await expect(page.getByText(names[1])).toBeVisible({ timeout: 8000 });

    await page.route("**/api/queries?*q=*", async (route) => {
      await new Promise((r) => setTimeout(r, 1200));
      await route.continue();
    });

    // A term not searched before: a cached result would skip the throttled refetch entirely.
    await search.fill("gamma");
    await expect(page.getByText(names[1])).toBeVisible();
    await expect(page.getByRole("status")).toBeVisible();

    await expect(page.getByText(names[1])).not.toBeVisible({ timeout: 8000 });
    await expect(page.getByText(names[2])).toBeVisible();

    await page.unroute("**/api/queries?*q=*");
    await search.fill("");
    await page.waitForTimeout(400);
    for (const name of names) {
      await expect(page.getByText(name)).toBeVisible({ timeout: 8000 });
    }
  } finally {
    for (const q of created) await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("dashboards search: exactly one debounced request; keepPreviousData avoids blank flash", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const names = [`e2e-dbnc-alpha-${stamp}`, `e2e-dbnc-beta-${stamp}`];
  const created = [];
  for (const name of names) created.push(await api.createDashboard(name));

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/dashboards");
    await expect(page.getByText(names[0])).toBeVisible({ timeout: 8000 });

    const search = page.getByPlaceholder("Search dashboards…");

    const reqUrls: string[] = [];
    const onReq = (req: import("@playwright/test").Request) => {
      const url = req.url();
      if (url.includes("/api/dashboards?") && url.includes("q=")) reqUrls.push(url);
    };
    page.on("request", onReq);

    await search.pressSequentially("alpha", { delay: 30 });
    await page.waitForTimeout(600);
    page.off("request", onReq);

    expect(reqUrls.length).toBe(1);
    expect(reqUrls[0]).toContain("q=alpha");
    await expect(page.getByText(names[0])).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(names[1])).not.toBeVisible();

    await search.fill("");
    await page.waitForTimeout(400);
    await expect(page.getByText(names[1])).toBeVisible({ timeout: 8000 });

    await page.route("**/api/dashboards?*q=*", async (route) => {
      await new Promise((r) => setTimeout(r, 1200));
      await route.continue();
    });

    await search.fill("beta");
    await expect(page.getByText(names[0])).toBeVisible();
    await expect(page.getByRole("status")).toBeVisible();

    await expect(page.getByText(names[0])).not.toBeVisible({ timeout: 8000 });
    await expect(page.getByText(names[1])).toBeVisible();

    await page.unroute("**/api/dashboards?*q=*");
    await search.fill("");
    await page.waitForTimeout(400);
    for (const name of names) {
      await expect(page.getByText(name)).toBeVisible({ timeout: 8000 });
    }
  } finally {
    for (const d of created) await api.deleteDashboard(d.slug).catch(() => {});
    await ctx.close();
  }
});
