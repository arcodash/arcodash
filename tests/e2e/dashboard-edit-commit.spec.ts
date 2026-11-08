/** E2E: dashboard edit/publish model — the workspace sees only the published snapshot until Publish
 * advances it. Requires: backend :8080, frontend :3000, target postgres :5434. */
import { test, expect, type Page, type Locator } from "@playwright/test";
import { adminContext, loginAs } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, executeAndWait, withTargetDs } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { clickActionBarItem, enterEditMode } from "./helpers/actions";
import { invitationToken } from "./helpers/permissions";

const PASSWORD = "DashCommitE2E123!";

/** Invite + accept a second org member and grant "Viewer" via a fresh group; a bare invited user
 * can't reach /dashboard/:slug (RequirePermission). Returns ids for teardown. */
async function createOrgMember(
  adminApi: E2EApiClient,
  name: string,
  email: string,
): Promise<{ userId: number; groupId: number }> {
  const invited = await adminApi.inviteUser({ name, email, groups: [] });
  const token = await invitationToken(invited.id);
  await new E2EApiClient().acceptInvitation(token, name, PASSWORD);

  const group = await adminApi.createGroup({ name: `dc-viewer-group-${Date.now()}-${invited.id}` });
  await adminApi.addGroupMember(group.id, invited.id);
  const roles = await adminApi.listRoles();
  const viewerRole = roles.find((r) => r.name === "Viewer" && r.type === "builtin");
  if (!viewerRole) throw new Error("builtin 'Viewer' role not found");
  await adminApi.addGroupRole(group.id, viewerRole.id);

  return { userId: invited.id, groupId: group.id };
}

async function enterViewMode(page: Page) {
  await page.getByRole("button", { name: "View", exact: true }).click();
}

/** Open the Publish confirm dialog (amber-banner button) and return it; label is "Publish", or
 * "Publish changes" once a published version exists. */
async function openPublishDialog(page: Page, label: "Publish" | "Publish changes"): Promise<Locator> {
  await page.getByRole("button", { name: label, exact: true }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  return dialog;
}

async function confirmDialog(dialog: Locator, label: string) {
  await dialog.getByRole("button", { name: label, exact: true }).click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
}

async function publishViaUi(page: Page, label: "Publish" | "Publish changes") {
  const dialog = await openPublishDialog(page, label);
  await confirmDialog(dialog, label);
}

async function discardViaUi(page: Page) {
  await page.getByRole("button", { name: "Discard changes", exact: true }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await confirmDialog(dialog, "Discard changes");
}

/** Add a text-only tile via the Add Widget dialog (no query dependency). */
async function addTextWidget(page: Page, text: string) {
  // Scope to the ActionBar: the empty-state placeholder has its own "Add Widget" button.
  await page.getByTestId("actionbar-root").getByRole("button", { name: "Add Widget", exact: true }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.getByRole("button", { name: "Text", exact: true }).click();
  await dialog.locator("textarea").fill(text);
  await dialog.getByRole("button", { name: "Add", exact: true }).click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
}

/** Click a widget card's delete (X) button (Edit mode only); the caller confirms Remove in the
 * ConfirmDialog. */
function widgetCard(page: Page, text: string) {
  return page.locator("div.rounded-md.border.border-border.bg-card").filter({ hasText: text });
}

test("edit a published dashboard's tiles stays invisible to the workspace until publish", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `dc-accept-b-${stamp}@example.com`;
  const { userId: bId, groupId: bGroupId } = await createOrgMember(adminApi, "DC Accept B", email);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  const keepText = `Tile Keep ${stamp}`;
  const removeText = `Tile Remove ${stamp}`;
  const newText = `Tile New ${stamp}`;

  const dash = await adminApi.createDashboard(`dc-accept-dash-${stamp}`);
  const w1 = await adminApi.createWidget({
    dashboard_id: dash.id,
    visualization_id: null,
    text: keepText,
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });
  const w2 = await adminApi.createWidget({
    dashboard_id: dash.id,
    visualization_id: null,
    text: removeText,
    options: { position: { col: 3, row: 0, sizeX: 3, sizeY: 2 } },
  });

  try {
    await adminPage.goto(`/dashboard/${dash.slug}`);
    await expect(adminPage.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });
    await enterEditMode(adminPage);
    await publishViaUi(adminPage, "Publish");
    await expect(adminPage.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

    await bPage.goto(`/dashboard/${dash.slug}`);
    await expect(bPage.getByText(keepText)).toBeVisible({ timeout: 8000 });
    await expect(bPage.getByText(removeText)).toBeVisible({ timeout: 8000 });

    await addTextWidget(adminPage, newText);
    await widgetCard(adminPage, removeText).locator("button.text-destructive").click();
    await adminPage.locator("[role=dialog]").getByRole("button", { name: "Remove", exact: true }).click();
    await expect(widgetCard(adminPage, removeText)).toHaveCount(0, { timeout: 8000 });
    await adminApi.saveDashboardLayout(dash.slug, [
      { widget_id: w1.id, position: { col: 1, row: 0, sizeX: 3, sizeY: 2 } },
    ]);

    // A direct API widget move doesn't invalidate the page's React Query cache; refetch first.
    await adminPage.reload();
    await enterEditMode(adminPage);
    await expect(adminPage.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

    await bPage.reload();
    await expect(bPage.getByText(keepText)).toBeVisible({ timeout: 8000 });
    await expect(bPage.getByText(removeText)).toBeVisible({ timeout: 8000 });
    await expect(bPage.getByText(newText)).toHaveCount(0, { timeout: 8000 });

    const dialog = await openPublishDialog(adminPage, "Publish changes");
    const summaryLine = "1 tile added · 1 removed · 1 moved";
    await expect(dialog.getByText(summaryLine, { exact: true })).toBeVisible({ timeout: 8000 });
    await confirmDialog(dialog, "Publish changes");
    await expect(adminPage.getByText("Unpublished changes")).toHaveCount(0, { timeout: 8000 });

    await bPage.reload();
    await expect(bPage.getByText(keepText)).toBeVisible({ timeout: 8000 });
    await expect(bPage.getByText(newText)).toBeVisible({ timeout: 8000 });
    await expect(bPage.getByText(removeText)).toHaveCount(0, { timeout: 8000 });
  } finally {
    await adminApi.deleteWidget(w2.id).catch(() => {});
    await adminApi.deleteDashboard(dash.slug).catch(() => {});
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});

test("discard changes restores the draft to the published arrangement", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const keepText = `Tile A ${stamp}`;
  const addedText = `Tile B ${stamp}`;

  const dash = await api.createDashboard(`dc-discard-dash-${stamp}`);
  await api.createWidget({
    dashboard_id: dash.id,
    visualization_id: null,
    text: keepText,
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });

  try {
    await page.goto(`/dashboard/${dash.slug}`);
    await enterEditMode(page);
    await publishViaUi(page, "Publish");

    await addTextWidget(page, addedText);
    await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(addedText)).toBeVisible({ timeout: 8000 });

    await discardViaUi(page);

    await expect(page.getByText(addedText)).toHaveCount(0, { timeout: 8000 });
    await expect(page.getByText(keepText)).toBeVisible();
    await expect(page.getByText("Unpublished changes")).toHaveCount(0, { timeout: 8000 });

    const rows = await queryRows<{ id: string }>(
      "SELECT id FROM widgets WHERE dashboard_id = $1",
      [dash.id],
    );
    expect(rows.length).toBe(1);
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
  }

  await ctx.close();
});

test("unpublish hides a dashboard from the workspace, and it can be published again", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `dc-unpub-b-${stamp}@example.com`;
  const { userId: bId, groupId: bGroupId } = await createOrgMember(adminApi, "DC Unpub B", email);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  const tileText = `Unpub Tile ${stamp}`;
  const dash = await adminApi.createDashboard(`dc-unpub-dash-${stamp}`);
  await adminApi.createWidget({
    dashboard_id: dash.id,
    visualization_id: null,
    text: tileText,
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });

  try {
    await adminPage.goto(`/dashboard/${dash.slug}`);
    await enterEditMode(adminPage);
    await publishViaUi(adminPage, "Publish");
    await expect(adminPage.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

    await bPage.goto(`/dashboard/${dash.slug}`);
    await expect(bPage.getByText(tileText)).toBeVisible({ timeout: 8000 });

    await clickActionBarItem(adminPage, "Unpublish");
    const dialog = adminPage.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await expect(dialog.getByText(/workspace loses access/i)).toBeVisible();
    await confirmDialog(dialog, "Unpublish");

    await expect(adminPage.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });

    // Must read as inaccessible, not silently blank.
    await bPage.reload();
    await expect(bPage.getByText(tileText)).toHaveCount(0, { timeout: 8000 });

    // Edit mode stays on after the first publish, so no "Edit" click is needed.
    await publishViaUi(adminPage, "Publish");
    await expect(adminPage.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });
    await bPage.reload();
    await expect(bPage.getByText(tileText)).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteDashboard(dash.slug).catch(() => {});
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});

test("a brand-new dashboard is invisible to the workspace until first publish, and Share/Embed are gated", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `dc-neverpub-b-${stamp}@example.com`;
  const { userId: bId, groupId: bGroupId } = await createOrgMember(adminApi, "DC Neverpub B", email);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  const name = `dc-neverpub-dash-${stamp}`;
  let slug = "";

  try {
    await page.goto("/dashboards");
    await page.click('button:has-text("New Dashboard")');
    const newDashDialog = page.locator("[role=dialog]");
    await expect(newDashDialog).toBeVisible({ timeout: 8000 });
    await newDashDialog.locator('input[placeholder="Dashboard name"]').fill(name);
    await newDashDialog.locator('button:has-text("Create")').click();
    await page.waitForURL(/\/dashboard\/([^/]+)/, { timeout: 10_000 });
    slug = page.url().match(/\/dashboard\/([^/]+)/)![1];

    // Hard navigation: an SPA transition from /dashboards can briefly leave two "Draft" badges for
    // getByText.
    await page.goto(`/dashboard/${slug}`);
    await expect(page.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });

    await enterEditMode(page);
    await expect(page.getByText(/This dashboard is a draft\./)).toBeVisible({ timeout: 8000 });

    // Publish's disabled-reason tooltip must be hoverable (not pointer-events:none).
    const publishBtn = page.getByRole("button", { name: "Publish", exact: true });
    await expect(publishBtn).toBeDisabled({ timeout: 8000 });
    await expect(publishBtn).toHaveAttribute("title", "Add at least one widget to publish");
    await publishBtn.hover();
    const publishIsHoverable = await publishBtn.evaluate((el) => {
      const rect = el.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return hit === el || el.contains(hit);
    });
    expect(publishIsHoverable).toBe(true);

    const shareBtn = page.getByRole("button", { name: "Share", exact: true });
    await expect(shareBtn).toBeDisabled({ timeout: 8000 });
    await expect(shareBtn).toHaveAttribute("title", "Publish this dashboard first");
    const embedBtn = page.getByRole("button", { name: "Public link", exact: true });
    await expect(embedBtn).toBeDisabled();
    await expect(embedBtn).toHaveAttribute("title", "Publish this dashboard first");

    await bPage.goto(`/dashboard/${slug}`);
    await expect(bPage.getByText("dashboard not found")).toBeVisible({ timeout: 8000 });

    await addTextWidget(page, `Tile ${stamp}`);
    await publishViaUi(page, "Publish");
    await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });
    await expect(shareBtn).toBeEnabled({ timeout: 8000 });
    await expect(embedBtn).toBeEnabled();

    await bPage.goto(`/dashboard/${slug}`);
    await expect(bPage.getByText(`Tile ${stamp}`)).toBeVisible({ timeout: 8000 });
  } finally {
    if (slug) await adminApi.deleteDashboard(slug).catch(() => {});
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await ctx.close();
    await bCtx.close();
  }
});

test("a dashboard tile shows its query's published result, unaffected by an unpublished query edit", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const stamp = Date.now();
    const name = `dc-compose-query-${stamp}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 42 AS val" });
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `dc-compose-viz-${stamp}`,
      options: {},
    });
    let dashSlug = "";
    try {
      await executeAndWait(api, q.id);

      await page.goto(`/queries/${q.id}`);
      await clickActionBarItem(page, "Publish");
      const qDialog = page.locator("[role=dialog]");
      await expect(qDialog).toBeVisible({ timeout: 8000 });
      await qDialog.getByRole("button", { name: "Publish", exact: true }).click();
      await expect(qDialog).not.toBeVisible({ timeout: 8000 });

      const dash = await api.createDashboard(`dc-compose-dash-${stamp}`);
      dashSlug = dash.slug;
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });

      await page.goto(`/dashboard/${dash.slug}`);
      await enterEditMode(page);
      await publishViaUi(page, "Publish");
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });

      await page.goto(`/queries/${q.id}`);
      await page.locator(".cm-content").click();
      await page.keyboard.press("ControlOrMeta+a");
      await page.keyboard.type("SELECT 99 AS val");
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("td").filter({ hasText: /^99$/ })).toHaveCount(0);
    } finally {
      if (dashSlug) await api.deleteDashboard(dashSlug).catch(() => {});
      await api.deleteVisualization(viz.id).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("a multi-tile layout save lands atomically — all positions present after reload, none partial", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();

  const dash = await api.createDashboard(`dc-layout-dash-${stamp}`);
  const texts = [`Tile 1 ${stamp}`, `Tile 2 ${stamp}`, `Tile 3 ${stamp}`];
  const widgets = await Promise.all(
    texts.map((text, i) =>
      api.createWidget({
        dashboard_id: dash.id,
        visualization_id: null,
        text,
        options: { position: { col: i * 2, row: 0, sizeX: 2, sizeY: 2 } },
      }),
    ),
  );

  try {
    // Hit the batched-save endpoint directly; a real react-grid-layout drag is too flaky.
    const newPositions = [
      { widget_id: widgets[0].id, position: { col: 1, row: 1, sizeX: 2, sizeY: 3 } },
      { widget_id: widgets[1].id, position: { col: 3, row: 1, sizeX: 2, sizeY: 3 } },
      { widget_id: widgets[2].id, position: { col: 0, row: 4, sizeX: 4, sizeY: 2 } },
    ];
    const response = await api.saveDashboardLayout(dash.slug, newPositions);
    expect(response.length).toBe(3);

    // bigint comes back as a string via node-pg.
    const rows = await queryRows<{ id: string; options: { position?: Record<string, number> } }>(
      "SELECT id, options FROM widgets WHERE dashboard_id = $1 ORDER BY id",
      [dash.id],
    );
    expect(rows.length).toBe(3);
    for (const expected of newPositions) {
      const row = rows.find((r) => Number(r.id) === expected.widget_id);
      expect(row?.options.position).toMatchObject(expected.position);
    }

    await page.goto(`/dashboard/${dash.slug}`);
    await enterEditMode(page);
    for (const text of texts) {
      await expect(page.getByText(text)).toBeVisible({ timeout: 10_000 });
    }
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
  }

  await ctx.close();
});

test("draft edits persist across reload; the workspace stays on the published version until publish", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const keepText = `Tile Base ${stamp}`;
  const addedText = `Tile Draft ${stamp}`;

  const dash = await api.createDashboard(`dc-reload-dash-${stamp}`);
  await api.createWidget({
    dashboard_id: dash.id,
    visualization_id: null,
    text: keepText,
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });

  try {
    await page.goto(`/dashboard/${dash.slug}`);
    await enterEditMode(page);
    await publishViaUi(page, "Publish");

    await addTextWidget(page, addedText);
    await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

    await page.reload();
    await enterEditMode(page);
    await expect(page.getByText(addedText)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

    await enterViewMode(page);
    await expect(page.getByText(keepText)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(addedText)).toHaveCount(0, { timeout: 8000 });
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
  }

  await ctx.close();
});

test("a never-published dashboard's View mode shows the owner's widgets, not the empty state", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const tileText = `Tile Never Published ${stamp}`;

  const dash = await api.createDashboard(`dc-neverpub-view-dash-${stamp}`);

  try {
    await page.goto(`/dashboard/${dash.slug}`);
    await enterEditMode(page);
    await addTextWidget(page, tileText);

    await enterViewMode(page);
    await expect(page.getByText(tileText)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Add your first widget")).toHaveCount(0, { timeout: 8000 });

    await enterEditMode(page);
    await expect(page.getByText(tileText)).toBeVisible({ timeout: 8000 });

    await publishViaUi(page, "Publish");
    await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });
    await enterViewMode(page);
    await expect(page.getByText(tileText)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Add your first widget")).toHaveCount(0, { timeout: 8000 });
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
  }

  await ctx.close();
});
