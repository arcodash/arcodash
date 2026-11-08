/** E2E: same-tab, no-reload session switch (admin -> logout -> limited-permission user), catching
 * stale client state surviving logout() (lib/auth.tsx). */
import { test, expect, type Page } from "@playwright/test";
import { loginAsAdmin } from "./helpers/api";
import { seedBuiltinRoleUser, seedCustomPermissionUser, seededUserContext } from "./helpers/permissions";
import { countRows } from "./helpers/db";
import { expectNoDeadEnd } from "./helpers/actions";


/** Sign out and log back in as another user on the same page via the rendered /login form;
 * loginAs()/page.goto() would reload the SPA and mask stale state. */
async function signOutAndLoginAs(
  page: Page,
  currentEmail: string,
  email: string,
  password: string,
): Promise<void> {
  await page.click(`button:has-text("${currentEmail}")`);
  await page.click('[role="menuitem"]:has-text("Sign out")');
  await page.waitForURL(/\/login/, { timeout: 8000 });
  await page.fill("#email", email);
  await page.fill("#password", password);
  await page.click('button[type="submit"]');
  await page.waitForURL((url) => !url.pathname.startsWith("/login"), { timeout: 10_000 });
}

test("A logs out and B logs in on the same page: nav/account identity flips, no dead end", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "session-switch", ["view_dashboard"]);
  const userA = await seedBuiltinRoleUser(adminApi, "switch-a", "Admin");

  const ctx = await seededUserContext(browser, userA);
  const page = await ctx.newPage();

  try {
    await page.goto("/queries");
    const sidebar = page.locator("aside nav");
    // 4 main entries + 4 Admin-section entries (Organization, Access Control, System Status, Audit
    // Log).
    await expect(sidebar.getByRole("link")).toHaveCount(8);
    await expect(page.locator(`button:has-text("${userA.email}")`)).toBeVisible({ timeout: 8000 });

    await signOutAndLoginAs(page, userA.email, user.email, user.password);
    await page.waitForURL(/\/dashboards/, { timeout: 8000 });

    await expect(sidebar.getByRole("link")).toHaveCount(1);
    await expect(sidebar.getByRole("link", { name: "Dashboards" })).toBeVisible();
    await expect(page.locator(`button:has-text("${user.email}")`)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(userA.email)).toHaveCount(0);

    await page.goto("/queries");
    await page.waitForURL(/\/dashboards/, { timeout: 8000 });
    await expectNoDeadEnd(page);
  } finally {
    await user.cleanup();
    await userA.cleanup();
    await ctx.close();
  }
});

test("B doesn't see A's per-user dashboard favorite state after a same-page session switch", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const dashName = `e2e-switch-dash-${stamp}`;
  const dash = await adminApi.createDashboard(dashName);
  const user = await seedCustomPermissionUser(adminApi, "switch-viewer", ["view_dashboard"]);
  const userA = await seedBuiltinRoleUser(adminApi, "switch-a", "Admin");
  // A never-published dashboard is visible only to its owner regardless of workspace visibility; an
  // explicit ACL grant bypasses that.
  await adminApi.rawFetch(`/api/dashboards/${dash.id}/acl`, {
    method: "POST",
    body: JSON.stringify({ grantee_id: user.userId, access_type: "viewer" }),
  });

  const ctx = await seededUserContext(browser, userA);
  const page = await ctx.newPage();

  try {
    await page.goto(`/dashboard/${dash.slug}`);
    const favBtn = page.getByTitle("Favorite");
    await expect(favBtn).toBeVisible({ timeout: 8000 });
    await favBtn.click();
    await expect
      .poll(() => countRows("favorites", "object_type = 'dashboard' AND object_id = $1", [dash.id]), {
        timeout: 8000,
      })
      .toBe(1);

    await page.goto("/dashboards");
    // The favorite star is in its own cell, a sibling of the name link; scope to the row.
    const rowA = page.locator("tr", { hasText: dashName });
    await expect(rowA).toBeVisible({ timeout: 8000 });
    await expect(rowA.locator("svg.fill-yellow-400")).toHaveCount(1);

    await signOutAndLoginAs(page, userA.email, user.email, user.password);
    await page.waitForURL(/\/dashboards/, { timeout: 8000 });

    const rowB = page.locator("tr", { hasText: dashName });
    await expect(rowB).toBeVisible({ timeout: 8000 });
    await expect(rowB.locator("svg.fill-yellow-400")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "New Dashboard" })).toHaveCount(0);

    // exact: true — a substring match also hits every row's own "Add to favorites" icon button.
    await page.getByRole("button", { name: "Favorites", exact: true }).click();
    await expect(page.getByText(dashName)).not.toBeVisible();
    await expect(page.getByText("No dashboards match these filters")).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteDashboard(dash.slug).catch(() => {});
    await user.cleanup();
    await userA.cleanup();
    await ctx.close();
  }
});
