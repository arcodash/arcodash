import { test, expect, type Page } from "@playwright/test";
import { adminContext, loginAs } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows, deleteRows } from "./helpers/db";
import { clickActionBarItem } from "./helpers/actions";
import { publishWake } from "./helpers/valkey";
import { invitationToken } from "./helpers/permissions";

const PASSWORD = "DashShareE2E123!";

// Polls the runs table for the newest row's terminal status, nudging the deadline-scheduler's wake
// channel.
async function waitForRunSuccess(queryId: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await publishWake().catch(() => {});
    const rows = await queryRows<{ status: string }>(
      "SELECT status FROM runs WHERE entity_type = 'query' AND entity_id = $1 ORDER BY id DESC LIMIT 1",
      [queryId],
    );
    if (rows[0]?.status === "success") return;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`no successful run appeared for query ${queryId} within ${timeoutMs}ms`);
}

// Invites and accepts a second member with the given builtin role (here "Editor": create_embed +
// edit_dashboard).
async function createOrgMemberWithRole(
  adminApi: E2EApiClient,
  name: string,
  email: string,
  roleName: "Editor" | "Viewer",
): Promise<{ userId: number; groupId: number }> {
  const invited = await adminApi.inviteUser({ name, email, groups: [] });
  const token = await invitationToken(invited.id);
  await new E2EApiClient().acceptInvitation(token, name, PASSWORD);

  const group = await adminApi.createGroup({ name: `${name}-base-${Date.now()}` });
  await adminApi.addGroupMember(group.id, invited.id);
  const roles = await adminApi.listRoles();
  const role = roles.find((r) => r.name === roleName && r.type === "builtin");
  if (!role) throw new Error(`builtin '${roleName}' role not found`);
  await adminApi.addGroupRole(group.id, role.id);

  return { userId: invited.id, groupId: group.id };
}

// Asserts no wide button or "More actions" item has this label; waits for data-dependent items
// first so an empty bar can't trivially pass.
async function expectActionBarItemAbsent(page: Page, label: string) {
  const actionBar = page.getByTestId("actionbar-root");
  await expect(actionBar.getByRole("button").first()).toBeVisible({ timeout: 8000 });
  const moreButton = actionBar.getByRole("button", { name: "More actions" });
  if (await moreButton.isVisible().catch(() => false)) {
    await moreButton.click();
    await expect(page.getByRole("menuitem", { name: label, exact: true })).toHaveCount(0);
    await page.keyboard.press("Escape");
  }
  await expect(actionBar.getByRole("button", { name: label, exact: true })).toHaveCount(0);
}


test("create dashboard → appears in list", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-dash-${Date.now()}`;

  await page.goto("/dashboards");
  await page.click('button:has-text("New Dashboard")');
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.locator('input[placeholder="Dashboard name"]').fill(name);
  await dialog.locator('button:has-text("Create")').click();
  await page.waitForURL(/\/dashboard\//, { timeout: 10_000 });
  await expect(page.getByText(name)).toBeVisible({ timeout: 8000 });

  const rows = await queryRows<{ slug: string }>(
    "SELECT slug FROM dashboards WHERE name = $1",
    [name],
  );
  if (rows[0]) {
    await deleteRows("dashboards", "slug = $1", [rows[0].slug]).catch(() => {});
  }
  await ctx.close();
});

test("share dashboard → public URL loads in incognito context", async ({ browser }) => {
  const api = await loginAsAdmin();

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-share-${Date.now()}`;

  await page.goto("/dashboards");
  await page.click('button:has-text("New Dashboard")');
  const newDashDialog = page.locator("[role=dialog]");
  await expect(newDashDialog).toBeVisible({ timeout: 8000 });
  await newDashDialog.locator('input[placeholder="Dashboard name"]').fill(name);
  await newDashDialog.locator('button:has-text("Create")').click();
  await page.waitForURL(/\/dashboard\/([^/]+)/, { timeout: 10_000 });
  const slug = page.url().match(/\/dashboard\/([^/]+)/)![1];

  // "Public link" is disabled until the dashboard is published, which needs a tile; a text widget
  // suffices.
  const dashboard = await api.getDashboard(slug);
  await api.createWidget({
    dashboard_id: dashboard.id,
    visualization_id: null,
    text: "placeholder",
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });
  await api.publishDashboard(slug);
  await page.reload();

  await clickActionBarItem(page, "Public link");
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.getByRole("checkbox", { name: /I understand/ }).check();
  await dialog.getByRole("button", { name: "Create public link" }).click();
  await expect(dialog.locator('input[readonly]')).toBeVisible({ timeout: 10_000 });
  const publicUrl = await dialog.locator('input[readonly]').inputValue();
  expect(publicUrl).toContain("/public/embed/");

  // SPA: check content, not HTTP status.
  const incognito = await browser.newContext();
  const publicPage = await incognito.newPage();
  await publicPage.goto(publicUrl);
  await expect(publicPage.getByText(name)).toBeVisible({ timeout: 8000 });

  await dialog.getByRole("button", { name: "Revoke" }).click();
  await expect(dialog.getByRole("button", { name: "Create public link" })).toBeVisible({
    timeout: 8000,
  });

  await publicPage.goto(publicUrl);
  await expect(
    publicPage.getByRole("heading", { name: "Link unavailable" }),
  ).toBeVisible({ timeout: 8000 });

  // A re-share after revoke must mint a new token; the old URL stays dead.
  await dialog.getByRole("checkbox", { name: /I understand/ }).check();
  await dialog.getByRole("button", { name: "Create public link" }).click();
  await expect(dialog.locator('input[readonly]')).toBeVisible({ timeout: 10_000 });
  const newPublicUrl = await dialog.locator('input[readonly]').inputValue();
  expect(newPublicUrl).toContain("/public/embed/");
  expect(newPublicUrl).not.toBe(publicUrl);

  await publicPage.goto(newPublicUrl);
  await expect(publicPage.getByText(name)).toBeVisible({ timeout: 8000 });

  await publicPage.goto(publicUrl);
  await expect(
    publicPage.getByRole("heading", { name: "Link unavailable" }),
  ).toBeVisible({ timeout: 8000 });

  await incognito.close();
  await ctx.close();

  await deleteRows("dashboards", "slug = $1", [slug]).catch(() => {});
});

test("auto-refresh re-reads the dashboard, so the freshness indicator's age actually advances", async ({
  browser,
}) => {
  test.setTimeout(150_000);
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const suffix = Date.now();

  const ds = await api.createDataSource({
    name: `e2e-autorefresh-ds-${suffix}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-autorefresh-q-${suffix}`,
    query: "SELECT 42 AS val",
  });
  const viz = await api.createVisualization({
    query_id: q.id,
    type: "table",
    name: `e2e-autorefresh-viz-${suffix}`,
    options: {},
  });
  const dash = await api.createDashboard(`e2e-autorefresh-${suffix}`);

  try {
    await api.publishQuery(q.id);
    await api.rawFetch(`/api/queries/${q.id}/run`, { method: "POST" });
    await waitForRunSuccess(q.id, 30_000);

    await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: viz.id,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await api.publishDashboard(dash.slug);

    await page.goto(`/dashboard/${dash.slug}`);
    await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });

    // The combobox trigger has no aria-label and is the only combobox on the page; match by role.
    await page.getByRole("combobox").click();
    await page.getByRole("option", { name: "10s", exact: true }).click();

    const freshDot = page.locator('span[title^="Updated "]');
    await expect(freshDot).toBeVisible();
    const before = await freshDot.getAttribute("title");

    // Only a real new execution should move the indicator; the dashboard never forces runs itself
    // (rate-limit storm).
    await api.rawFetch(`/api/queries/${q.id}/run`, { method: "POST" });
    await waitForRunSuccess(q.id, 30_000);

    // Wait past one 10s auto-refresh tick.
    await expect
      .poll(() => freshDot.getAttribute("title"), { timeout: 20_000, intervals: [1000] })
      .not.toBe(before);
  } finally {
    await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(() => {});
    await api.deleteDashboard(dash.slug).catch(() => {});
    await api.deleteVisualization(viz.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("Share action follows can_share: the creator sees it and it works, a non-creator Editor does not", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `e2e-dash-share-editor-${stamp}@example.com`;
  const { userId: bId, groupId: bGroupId } = await createOrgMemberWithRole(
    adminApi,
    "E2E Dash Share Editor",
    email,
    "Editor",
  );

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  const name = `e2e-dash-share-${stamp}`;
  let slug = "";

  try {
    // Admin is both creator and org admin, so either half passes; the boolean matrix is tested
    // elsewhere, this proves the UI wiring.
    await page.goto("/dashboards");
    await page.click('button:has-text("New Dashboard")');
    const newDashDialog = page.locator("[role=dialog]");
    await expect(newDashDialog).toBeVisible({ timeout: 8000 });
    await newDashDialog.locator('input[placeholder="Dashboard name"]').fill(name);
    await newDashDialog.locator('button:has-text("Create")').click();
    await page.waitForURL(/\/dashboard\/([^/]+)/, { timeout: 10_000 });
    slug = page.url().match(/\/dashboard\/([^/]+)/)![1];

    const dashboard = await adminApi.getDashboard(slug);
    await adminApi.createWidget({
      dashboard_id: dashboard.id,
      visualization_id: null,
      text: "placeholder",
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await adminApi.publishDashboard(slug);
    await page.reload();

    // Flip visibility to prove Share is usable (no 403), then flip back so B's half sees the
    // dashboard.
    await clickActionBarItem(page, "Share");
    const shareDialog = page.locator("[role=dialog]");
    await expect(shareDialog).toBeVisible({ timeout: 8000 });
    await shareDialog.getByRole("button", { name: "Specific people" }).click();
    await expect(shareDialog.getByText("Not shared with anyone yet.")).toBeVisible({ timeout: 8000 });
    await shareDialog.getByRole("button", { name: "Everyone" }).click();
    await expect(shareDialog.getByText("Visible to your whole workspace.")).toBeVisible({ timeout: 8000 });
    await expect(shareDialog.locator(".text-destructive")).toHaveCount(0);
    await shareDialog.locator('button:has-text("Done")').click();

    // Non-creator Editor: "Share" must be absent from the ActionBar, not disabled.
    await bPage.goto(`/dashboard/${slug}`);
    await expect(bPage.getByText(name)).toBeVisible({ timeout: 8000 });
    await expectActionBarItemAbsent(bPage, "Share");
    await expect(bPage.locator(".text-destructive")).toHaveCount(0);
  } finally {
    if (slug) await adminApi.deleteDashboard(slug).catch(() => {});
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await ctx.close();
    await bCtx.close();
  }
});
