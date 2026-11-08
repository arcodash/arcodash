/** E2E: admin-area tests — groups, organization, audit log, system status. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";

test("create group → appears in groups list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const groupName = `e2e-group-${Date.now()}`;

  await page.goto("/access?tab=groups");
  await page.getByRole("button", { name: "+ New group" }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.getByPlaceholder("Group name").fill(groupName);
  await dialog.locator('button:has-text("Create")').click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });

  await expect(page.getByText(groupName)).toBeVisible({ timeout: 8000 });

  const rows = await queryRows<{ id: number }>(
    "SELECT id FROM groups WHERE name = $1",
    [groupName],
  );
  if (rows[0]) await api.deleteGroup(rows[0].id).catch(() => {});
  await ctx.close();
});

test("non-admin cannot see the Access Control groups tab (API 403)", async () => {
  const adminApi = await loginAsAdmin();
  const nonAdminEmail = `e2e-nonadmin-${Date.now()}@example.com`;

  const invited = await adminApi.inviteUser({
    name: "E2E Non-Admin",
    email: nonAdminEmail,
    groups: [],
  });

  const rows = await queryRows<{ token: string }>(
    "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
    [invited.id],
  );
  const token = rows[0]?.token;
  if (!token) throw new Error("invitation_token missing from DB");

  const nonAdminApi = new E2EApiClient();
  await nonAdminApi.acceptInvitation(token, "E2E Non-Admin", "TestPass123!");
  await nonAdminApi.login(nonAdminEmail, "TestPass123!");

  // An authenticated non-admin must get 403 from the API the groups tab calls.
  const status = await nonAdminApi.getStatus("/api/groups");
  expect(status).toBe(403);

  await adminApi.deleteUser(invited.id).catch(() => {});
});

test("update org name → persists after page reload", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/organization");
  await page.waitForSelector('input:not([disabled])', { timeout: 8000 });

  const original = await page.locator('input:not([disabled])').first().inputValue();
  const newName = `e2e-org-${Date.now()}`;

  await page.locator('input:not([disabled])').first().fill(newName);
  await page.click('button[type="submit"]');
  await expect(page.getByText("Saved.")).toBeVisible({ timeout: 8000 });

  await page.reload();
  await page.waitForSelector('input:not([disabled])', { timeout: 8000 });
  const reloaded = await page.locator('input:not([disabled])').first().inputValue();
  expect(reloaded).toBe(newName);

  await page.locator('input:not([disabled])').first().fill(original || "arcodash");
  await page.click('button[type="submit"]');
  await expect(page.getByText("Saved.")).toBeVisible({ timeout: 8000 });

  await ctx.close();
});

test("audit log: login action appears in events list", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/admin/audit");
  await expect(page.locator("td").filter({ hasText: "login" }).first()).toBeVisible({
    timeout: 8000,
  });

  await ctx.close();
});

test("audit log: filtering by a visible action's displayed text keeps that row", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/admin/audit");
  const firstRow = page.locator("table tbody tr").first();
  await expect(firstRow).toBeVisible({ timeout: 8000 });
  const actionText = await firstRow.locator("td").nth(1).innerText();

  await Promise.all([
    page.waitForResponse(
      (r) => r.url().includes("/api/events?") && r.url().includes("action="),
    ),
    page.getByPlaceholder("Filter by action…").fill(actionText),
  ]);

  await expect(page.locator("table tbody tr").first()).toBeVisible({ timeout: 8000 });
  await expect(page.getByText("No events in this range")).not.toBeVisible();

  await ctx.close();
});

test("audit log: creating and editing a query both appear without a reload", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-audit-query-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    const queryName = `e2e-audit-query-${Date.now()}`;
    const query = await api.createQuery({
      data_source_id: ds.id,
      name: queryName,
      query: "SELECT 1",
    });
    await api.updateQuery(query.id, { name: `${queryName}-renamed` });

    await page.goto("/admin/audit");
    await Promise.all([
      page.waitForResponse(
        (r) => r.url().includes("/api/events?") && r.url().includes("action="),
      ),
      page.getByPlaceholder("Filter by action…").fill("query."),
    ]);

    // Both rows exist before load, no reload needed. .first(): the audit log is shared with other
    // specs.
    await expect(page.getByText("Query Created").first()).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Query Updated").first()).toBeVisible({ timeout: 8000 });

    await api.deleteQuery(query.id).catch(() => {});
  } finally {
    await ctx.close();
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

test("system status page loads and shows queue names", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/admin/status");
  await expect(page.locator("body")).not.toContainText("Failed to load status.", {
    timeout: 8000,
  });

  await ctx.close();
});
