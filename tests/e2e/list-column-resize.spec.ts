/**
 * E2E: the Name column on the queries list can be widened by dragging its header handle.
 */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

test("queries list: drag widens Name, persists across reload, double-click resets", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await page.setViewportSize({ width: 1000, height: 800 });

  const ds = await api.createDataSource({
    name: `e2e-colresize-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `colresize-${"very-long-name-".repeat(12)}${Date.now()}`,
    query: "SELECT 1",
  });
  try {
    await page.goto("/queries");
    const head = page.getByRole("columnheader").first();
    const handle = page.getByRole("separator", { name: "Resize Name column" });
    await expect(handle).toBeVisible({ timeout: 8000 });
    await page.evaluate(() => localStorage.removeItem("arcodash.listcol.queries.name"));
    await page.reload();
    await expect(handle).toBeVisible();

    const before = (await head.boundingBox())!.width;
    const hb = (await handle.boundingBox())!;
    const x = hb.x + hb.width / 2;
    const y = hb.y + hb.height / 2;
    await page.mouse.move(x, y);
    await page.mouse.down();
    await page.mouse.move(x + 150, y, { steps: 5 });
    await page.mouse.up();

    await expect.poll(async () => (await head.boundingBox())!.width).toBeGreaterThan(before + 100);
    const widened = (await head.boundingBox())!.width;

    await page.reload();
    await expect(handle).toBeVisible();
    await expect.poll(async () => Math.abs((await head.boundingBox())!.width - widened)).toBeLessThan(2);

    await handle.dblclick();
    await expect.poll(async () => (await head.boundingBox())!.width).toBeLessThan(widened - 100);
    expect(await page.evaluate(() => localStorage.getItem("arcodash.listcol.queries.name"))).toBeNull();
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
