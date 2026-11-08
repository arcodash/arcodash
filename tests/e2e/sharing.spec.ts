/** E2E: sharing — restricted-dashboard grant/revoke, Editor grant on a query via a group, query
 * draft privacy. */
import { test, expect } from "@playwright/test";
import { adminContext, loginAs } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, withTargetDs } from "./helpers/api";
import { clickActionBarItem } from "./helpers/actions";
import { invitationToken } from "./helpers/permissions";

const PASSWORD = "SharingE2E123!";

/** Invite + accept a second org member via API; returns their user id and a fresh group (caller
 * deletes) with the builtin "Viewer" role. Routes need an org-level permission via a role on the
 * group, else every page redirects to /settings. */
async function createOrgMember(
  adminApi: E2EApiClient,
  name: string,
  email: string,
): Promise<{ userId: number; groupId: number }> {
  const invited = await adminApi.inviteUser({ name, email, groups: [] });
  const token = await invitationToken(invited.id);
  await new E2EApiClient().acceptInvitation(token, name, PASSWORD);

  const group = await adminApi.createGroup({ name: `${name}-base-${Date.now()}` });
  await adminApi.addGroupMember(group.id, invited.id);
  const roles = await adminApi.listRoles();
  const viewerRole = roles.find((r) => r.name === "Viewer" && r.type === "builtin");
  if (!viewerRole) throw new Error("builtin 'Viewer' role not found");
  await adminApi.addGroupRole(group.id, viewerRole.id);

  return { userId: invited.id, groupId: group.id };
}

test("restricted dashboard: grant Viewer to a second org member, then revoke", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `e2e-share-dashb-${stamp}@example.com`;
  const { userId: bId, groupId: bGroupId } = await createOrgMember(adminApi, "E2E Share Dash B", email);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  const name = `e2e-share-dashboard-${stamp}`;
  let dashSlug = "";
  let dashId = 0;
  let grantId: number | null = null;

  try {
    await adminPage.goto("/dashboards");
    await adminPage.click('button:has-text("New Dashboard")');
    const newDashDialog = adminPage.locator("[role=dialog]");
    await expect(newDashDialog).toBeVisible({ timeout: 8000 });
    await newDashDialog.locator('input[placeholder="Dashboard name"]').fill(name);
    await newDashDialog.locator('button:has-text("Create")').click();
    await adminPage.waitForURL(/\/dashboard\/([^/]+)/, { timeout: 10_000 });
    dashSlug = adminPage.url().match(/\/dashboard\/([^/]+)/)![1];
    dashId = (await adminApi.getDashboard(dashSlug)).id;

    // A never-published dashboard is invisible regardless of ACL; publish it first.
    await adminApi.createWidget({
      dashboard_id: dashId,
      visualization_id: null,
      text: "placeholder",
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await adminApi.publishDashboard(dashSlug);

    await bPage.goto("/dashboards");
    await expect(bPage.getByText(name)).toBeVisible({ timeout: 8000 });

    await adminApi.rawFetch(`/api/dashboards/${dashId}/visibility`, {
      method: "PUT",
      body: JSON.stringify({ visibility: "restricted" }),
    });

    await bPage.goto("/dashboards");
    await expect(bPage.getByText(name)).toHaveCount(0);
    await bPage.goto(`/dashboard/${dashSlug}`);
    await expect(bPage.getByText("dashboard not found")).toBeVisible({ timeout: 8000 });

    const grant = await adminApi.rawFetch<{ id: number }>(`/api/dashboards/${dashId}/acl`, {
      method: "POST",
      body: JSON.stringify({ grantee_id: bId, access_type: "viewer" }),
    });
    grantId = grant.id;

    await bPage.goto("/dashboards");
    await expect(bPage.getByText(name)).toBeVisible({ timeout: 8000 });
    await bPage.goto(`/dashboard/${dashSlug}`);
    await expect(bPage.getByText(name)).toBeVisible({ timeout: 8000 });

    await adminApi.rawFetch(`/api/dashboards/${dashId}/acl/${grantId}`, { method: "DELETE" });
    grantId = null;

    await bPage.goto("/dashboards");
    await expect(bPage.getByText(name)).toHaveCount(0);
    await bPage.goto(`/dashboard/${dashSlug}`);
    await expect(bPage.getByText("dashboard not found")).toBeVisible({ timeout: 8000 });
  } finally {
    if (grantId != null) {
      await adminApi
        .rawFetch(`/api/dashboards/${dashId}/acl/${grantId}`, { method: "DELETE" })
        .catch(() => {});
    }
    if (dashSlug) await adminApi.deleteDashboard(dashSlug).catch(() => {});
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});

test("group grant on a query: Editor access via a group, then dropped", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `e2e-share-queryb-${stamp}@example.com`;
  // Separate group, only the target of the per-query Editor grant, so dropping it keeps B's
  // baseline access.
  const { userId: bId, groupId: bBaseGroupId } = await createOrgMember(adminApi, "E2E Share Query B", email);
  const group = await adminApi.createGroup({ name: `e2e-share-group-${stamp}` });
  await adminApi.addGroupMember(group.id, bId);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  try {
    await withTargetDs(adminApi, async (dsId) => {
      const name = `e2e-share-query-${stamp}`;
      const q = await adminApi.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
      try {
        // Draft with no grant: the editor shell never mounts on a load error; "query not found"
        // renders instead.
        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.getByText("query not found")).toBeVisible({ timeout: 8000 });

        await adminPage.goto(`/queries/${q.id}`);
        await clickActionBarItem(adminPage, "Share");
        const dialog = adminPage.locator("[role=dialog]");
        await expect(dialog).toBeVisible({ timeout: 8000 });
        await dialog.getByRole("combobox", { name: "Add person or group" }).click();
        await adminPage.getByRole("option", { name: `${group.name} (group)`, exact: true }).click();
        await expect(dialog.getByText(group.name)).toBeVisible({ timeout: 8000 });

        const roleSelect = dialog.getByRole("combobox", { name: `Role for ${group.name}` });
        await roleSelect.click();
        await adminPage.getByRole("option", { name: "Editor", exact: true }).click();
        await expect(roleSelect).toHaveText("Editor", { timeout: 8000 });
        await dialog.locator('button:has-text("Done")').click();

        // An explicit grant bypasses draft/visibility gating.
        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.locator("#query-name")).toHaveValue(name, { timeout: 8000 });

        await adminPage.goto(`/queries/${q.id}`);
        await clickActionBarItem(adminPage, "Share");
        await expect(dialog).toBeVisible({ timeout: 8000 });
        await dialog.getByRole("button", { name: `Remove ${group.name}` }).click();
        await expect(dialog.getByText(group.name)).toHaveCount(0);
        await dialog.locator('button:has-text("Done")').click();

        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.getByText("query not found")).toBeVisible({ timeout: 8000 });
      } finally {
        await adminApi.deleteQuery(q.id).catch(() => {});
      }
    });
  } finally {
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(group.id).catch(() => {});
    await adminApi.deleteGroup(bBaseGroupId).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});

test("disabling a user hides them from the Share picker while UsersPage keeps showing them as Disabled", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `e2e-share-disabled-${stamp}@example.com`;
  // Stamped because the test asserts an option's ABSENCE; a static name could collide with a
  // leftover user.
  const name = `E2E Share Disabled ${stamp}`;
  const { userId: bId, groupId: bGroupId } = await createOrgMember(adminApi, name, email);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await row.getByRole("button", { name: "Disable" }).click();
    const disableDialog = page.locator("[role=dialog]");
    await expect(disableDialog).toBeVisible({ timeout: 8000 });
    await disableDialog.locator('button:has-text("Disable")').click();
    // exact:true: the email itself contains "disabled".
    await expect(row.getByText("Disabled", { exact: true })).toBeVisible({ timeout: 8000 });

    await withTargetDs(adminApi, async (dsId) => {
      const queryName = `e2e-share-disabled-query-${stamp}`;
      const q = await adminApi.createQuery({ data_source_id: dsId, name: queryName, query: "SELECT 1" });
      try {
        await page.goto(`/queries/${q.id}`);
        await clickActionBarItem(page, "Share");
        const shareDialog = page.locator("[role=dialog]");
        await expect(shareDialog).toBeVisible({ timeout: 8000 });
        await shareDialog.getByRole("combobox", { name: "Add person or group" }).click();
        await expect(page.getByRole("option", { name, exact: true })).toHaveCount(0);
        await page.keyboard.press("Escape");
        await shareDialog.locator('button:has-text("Done")').click();
      } finally {
        await adminApi.deleteQuery(q.id).catch(() => {});
      }
    });

    await page.goto("/users");
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Disabled", { exact: true })).toBeVisible();
  } finally {
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(bGroupId).catch(() => {});
    await ctx.close();
  }
});

test("draft privacy: a draft query is hidden until published", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `e2e-draft-b-${stamp}@example.com`;
  // Workspace visibility needs the viewer's group to have data-source access, so B gets a second
  // group with it.
  const { userId: bId, groupId: bBaseGroupId } = await createOrgMember(adminApi, "E2E Draft B", email);
  const group = await adminApi.createGroup({ name: `e2e-draft-group-${stamp}` });
  await adminApi.addGroupMember(group.id, bId);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  try {
    await withTargetDs(adminApi, async (dsId) => {
      await adminApi.rawFetch(`/api/groups/${group.id}/data_sources`, {
        method: "POST",
        body: JSON.stringify({ data_source_id: dsId }),
      });
      const name = `e2e-draft-query-${stamp}`;
      const q = await adminApi.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
      try {
        await bPage.goto("/queries");
        await expect(bPage.getByText(name)).toHaveCount(0);
        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.getByText("query not found")).toBeVisible({ timeout: 8000 });

        await adminPage.goto(`/queries/${q.id}`);
        await expect(adminPage.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });
        await clickActionBarItem(adminPage, "Publish");
        const publishDialog = adminPage.locator("[role=dialog]");
        await expect(publishDialog).toBeVisible({ timeout: 8000 });
        await publishDialog.getByRole("button", { name: "Publish", exact: true }).click();
        await expect(adminPage.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

        await bPage.goto("/queries");
        await expect(bPage.getByText(name)).toBeVisible({ timeout: 8000 });
        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.locator("#query-name")).toHaveValue(name, { timeout: 8000 });
      } finally {
        await adminApi.deleteQuery(q.id).catch(() => {});
      }
    });
  } finally {
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(group.id).catch(() => {});
    await adminApi.deleteGroup(bBaseGroupId).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});

test("Share & Embed dialog: public link row does not overflow the dialog horizontally", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  // Name avoids "embed": the title-rename button's accessible name would collide with the "Public
  // link" button.
  const dash = await adminApi.createDashboard(`e2e-share-overflow-${Date.now()}`);
  try {
    await adminApi.createWidget({
      dashboard_id: dash.id,
      visualization_id: null,
      text: "placeholder",
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await adminApi.publishDashboard(dash.slug);

    await page.goto(`/dashboard/${dash.slug}`);
    await clickActionBarItem(page, "Public link");
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });

    await dialog.getByRole("checkbox", { name: /I understand/ }).check();
    await dialog.getByRole("button", { name: "Create public link" }).click();
    await expect(dialog.getByText(/active public link/)).toBeVisible({ timeout: 8000 });

    // The 64-hex token makes the URL long enough to overflow the dialog's max-w-lg (grid items
    // default to min-width: auto).
    await expect(dialog.locator("input[readonly]")).toBeVisible();
    const overflowsHorizontally = await dialog.evaluate(
      (el) => el.scrollWidth > el.clientWidth + 1,
    );
    expect(overflowsHorizontally).toBe(false);
  } finally {
    await adminApi.deleteDashboard(dash.slug).catch(() => {});
    await ctx.close();
  }
});
