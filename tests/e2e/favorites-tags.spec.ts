// Favorites is polymorphic (object_type/object_id/user_id); assert object_type = 'dashboard'. The
// star toggle exists only on the detail page.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { countRows } from "./helpers/db";

test("tag pill filters dashboards list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const tag = `e2e-tag-${stamp}`;

  const tagged = await api.createDashboard(`e2e-tagged-${stamp}`);
  const untagged = await api.createDashboard(`e2e-untagged-${stamp}`);
  await api.updateDashboard(tagged.slug, { tags: [tag] });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/dashboards");
    await expect(page.getByText(tagged.name)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(untagged.name)).toBeVisible({ timeout: 8000 });

    const pill = page.getByRole("button", { name: tag, exact: true });
    await expect(pill).toBeVisible({ timeout: 8000 });
    await pill.click();

    await expect(page.getByText(tagged.name)).toBeVisible();
    await expect(page.getByText(untagged.name)).not.toBeVisible();

    await page.getByRole("button", { name: "clear" }).click();
    await expect(page.getByText(untagged.name)).toBeVisible({ timeout: 8000 });
  } finally {
    await api.deleteDashboard(tagged.slug).catch(() => {});
    await api.deleteDashboard(untagged.slug).catch(() => {});
    await ctx.close();
  }
});

test("star dashboard on detail page → favorites filter shows it; unstar removes it; DB rows match", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const dash = await api.createDashboard(`e2e-fav-${Date.now()}`);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto(`/dashboard/${dash.slug}`);
    const favBtn = page.getByTitle("Favorite");
    await expect(favBtn).toBeVisible({ timeout: 8000 });
    await favBtn.click();

    await expect
      .poll(
        () =>
          countRows("favorites", "object_type = 'dashboard' AND object_id = $1", [dash.id]),
        { timeout: 8000 },
      )
      .toBe(1);

    await page.goto("/dashboards");
    await page.getByRole("button", { name: "Favorites", exact: true }).click();
    await expect(page.getByText(dash.name)).toBeVisible({ timeout: 8000 });

    await page.goto(`/dashboard/${dash.slug}`);
    await page.getByTitle("Favorite").click();

    await expect
      .poll(
        () =>
          countRows("favorites", "object_type = 'dashboard' AND object_id = $1", [dash.id]),
        { timeout: 8000 },
      )
      .toBe(0);

    await page.goto("/dashboards");
    await page.getByRole("button", { name: "Favorites", exact: true }).click();
    await expect(page.getByText(dash.name)).not.toBeVisible({ timeout: 8000 });
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
    await ctx.close();
  }
});
