// Requires: backend :8080, frontend :3000. Allowed controls must work; forbidden ones must be
// absent, not disabled.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, TARGET_DS_OPTIONS } from "./helpers/api";
import { seedCustomPermissionUser, seedBuiltinRoleUser, seededUserContext } from "./helpers/permissions";
import { queryRows } from "./helpers/db";
import { expectNoDeadEnd } from "./helpers/actions";


test("users-mgr: Users + Groups tabs and Invite visible; group picker lists groups (manage_users implies view_groups); Roles absent; stale ?tab redirects", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "users-mgr", ["manage_users"]);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto("/access?tab=users");
    await expect(page.getByRole("heading", { name: "Access Control" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("tab", { name: "Users" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("tab", { name: "Groups" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Roles" })).toHaveCount(0);
    const inviteBtn = page.getByRole("button", { name: "+ Invite User" });
    await expect(inviteBtn).toBeVisible({ timeout: 8000 });

    // Send needs an explicit group pick; manage_users implies view_groups.
    await inviteBtn.click();
    // Named dialog: the closed group-picker Popover also has role="dialog" and lingers in the DOM.
    const dialog = page.getByRole("dialog", { name: "Invite User" });
    await dialog.getByPlaceholder("Jane Smith").fill("Invitee");
    await dialog.getByPlaceholder("jane@example.com").fill(`e2e-users-mgr-invitee-${Date.now()}@example.com`);
    const sendBtn = dialog.getByRole("button", { name: "Send Invitation", exact: true });
    await expect(sendBtn).toBeDisabled();
    await dialog.getByRole("button", { name: "Add group…" }).click();
    await expect(page.getByText("No groups found.")).toHaveCount(0);
    await page.locator("label[title]").first().click();
    await page.keyboard.press("Escape"); // close the popover, not the dialog underneath it
    await expect(sendBtn).toBeEnabled();
    await dialog.getByRole("button", { name: "Close" }).first().click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    await page.goto("/access?tab=roles");
    await page.waitForURL(/tab=users/, { timeout: 8000 });
    await expectNoDeadEnd(page);
  } finally {
    await user.cleanup();
    await ctx.close();
  }
});


test("groups-mgr: only Groups tab; can read roles to attach; Roles tab absent", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "groups-mgr", ["manage_groups"]);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();
  const groupName = `e2e-groups-mgr-created-${Date.now()}`;

  try {
    await page.goto("/access?tab=groups");
    await expect(page.getByRole("heading", { name: "Access Control" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("tab", { name: "Groups" })).toHaveCount(0); // single tab → no tab bar
    await expect(page.getByRole("button", { name: "+ New group" })).toBeVisible({ timeout: 8000 });

    await page.getByRole("button", { name: "+ New group" }).click();
    const createDialog = page.locator("[role=dialog]");
    await createDialog.getByPlaceholder("Group name").fill(groupName);
    await createDialog.locator('button:has-text("Create")').click();
    await expect(createDialog).not.toBeVisible({ timeout: 8000 });
    const groupRow = page.getByRole("row", { name: new RegExp(groupName) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });

    await groupRow.getByRole("button", { name: "Manage" }).click();
    const manageDialog = page.locator("[role=dialog]");
    await manageDialog.getByRole("button", { name: "Attach role…" }).click();
    await expect(page.getByPlaceholder("Search…")).toBeVisible({ timeout: 8000 });
    await page.keyboard.press("Escape");
    await manageDialog.locator('button:has-text("Close")').first().click();

    await expect(page.getByRole("tab", { name: "Roles" })).toHaveCount(0);
    await page.goto("/access?tab=roles");
    await page.waitForURL(/tab=groups/, { timeout: 8000 });
    await expectNoDeadEnd(page);
  } finally {
    const rows = await adminApi.listGroups();
    const g = rows.find((r) => r.name === groupName);
    if (g) await adminApi.deleteGroup(g.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});


test("roles-mgr: only Roles tab; create role works; Groups/Users absent", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "roles-mgr", ["manage_roles"]);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();
  const roleName = `e2e-roles-mgr-created-${Date.now()}`;

  try {
    await page.goto("/access?tab=roles");
    await expect(page.getByRole("heading", { name: "Access Control" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("tab", { name: "Roles" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "+ New role" })).toBeVisible({ timeout: 8000 });

    await page.getByRole("button", { name: "+ New role" }).click();
    const dialog = page.locator("[role=dialog]");
    await dialog.getByLabel("Name", { exact: true }).fill(roleName);
    // A role may only grant permissions its author holds; manage_roles implies view_roles.
    await dialog.getByLabel("View roles", { exact: true }).check();
    await dialog.locator('button:has-text("Save")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("row", { name: new RegExp(roleName) })).toBeVisible({ timeout: 8000 });

    await expect(page.getByRole("tab", { name: "Groups" })).toHaveCount(0);
    await expect(page.getByRole("tab", { name: "Users" })).toHaveCount(0);
    await page.goto("/access?tab=users");
    await page.waitForURL(/tab=roles/, { timeout: 8000 });
    await expectNoDeadEnd(page);
  } finally {
    const rows = await adminApi.listRoles();
    const r = rows.find((x) => x.name === roleName);
    if (r) await adminApi.deleteRole(r.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});


test("dashboards-editor: Dashboards/Queries/Data Sources nav present; DS create/edit hidden; read-only Users tab", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedBuiltinRoleUser(adminApi, "editor", "Editor");
  const ds = await adminApi.createDataSource({
    name: `e2e-editor-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  // Data source visibility needs view_data_source plus a reach grant.
  await adminApi.assignGroupDataSource(user.groupId, ds.id);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto("/dashboards");
    await expect(page.getByRole("link", { name: "Dashboards" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("link", { name: "Queries" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Data Sources" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Monitors" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Destinations" })).toHaveCount(0); // no manage_destinations
    await expect(page.getByRole("button", { name: "New Dashboard" })).toBeVisible();

    await page.goto("/data_sources");
    await expect(page.getByText(ds.name)).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("button", { name: "+ New data source" })).toHaveCount(0);
    // Exact match: the DS card title ("e2e-editor-ds-...") otherwise substring-matches "Edit".
    await expect(page.getByRole("link", { name: "Edit", exact: true })).toHaveCount(0);

    await page.goto("/access?tab=users");
    await expectNoDeadEnd(page);
    await expect(page.getByRole("button", { name: "+ Invite User" })).toHaveCount(0);
    await expect(page.getByRole("columnheader", { name: "Actions" })).toHaveCount(0);
  } finally {
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});


test("viewer: view-only nav; no create/edit anywhere; /access and /queries/new redirect away", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedBuiltinRoleUser(adminApi, "viewer", "Viewer");
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto("/dashboards");
    await expect(page.getByRole("link", { name: "Dashboards" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("button", { name: "New Dashboard" })).toHaveCount(0);

    await page.goto("/queries");
    await expect(page.getByRole("link", { name: "New Query" })).toHaveCount(0);

    await page.goto("/monitors");
    await expect(page.getByRole("button", { name: "+ New Monitor" })).toHaveCount(0);

    await page.goto("/data_sources");
    await expect(page.getByRole("button", { name: "+ New data source" })).toHaveCount(0);

    await page.goto("/access?tab=users");
    await page.waitForURL((url) => !url.pathname.startsWith("/access"), { timeout: 8000 });
    await expectNoDeadEnd(page);

    await page.goto("/queries/new");
    await page.waitForURL((url) => url.pathname !== "/queries/new", { timeout: 8000 });
    await expectNoDeadEnd(page);
    await expect(page.locator(".cm-editor")).toHaveCount(0);
  } finally {
    await user.cleanup();
    await ctx.close();
  }
});


test("no-access: Settings shows only Personal; /access and /admin/status silently redirect (no error page)", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "no-access", []);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto("/settings");
    await expect(page.getByRole("heading", { name: "Settings" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByText("Personal")).toBeVisible();
    await expect(page.getByText("Workspace")).toHaveCount(0);
    await expect(page.getByText("System", { exact: true })).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Access Control" })).toHaveCount(0);

    // Exact match: the Notifications card text ("...monitors you follow.") otherwise
    // substring-matches "Monitors".
    for (const label of ["Queries", "Data Sources", "Dashboards", "Monitors", "Destinations"]) {
      await expect(page.getByRole("link", { name: label, exact: true })).toHaveCount(0);
    }

    await page.goto("/access");
    await page.waitForURL(/\/settings/, { timeout: 8000 });
    await expectNoDeadEnd(page);

    await page.goto("/admin/status");
    await page.waitForURL(/\/settings/, { timeout: 8000 });
    await expectNoDeadEnd(page);
  } finally {
    await user.cleanup();
    await ctx.close();
  }
});


test("admin: everything visible", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/settings");
    // Scope to page content; the sidebar shows the same labels.
    const main = page.locator("main");
    await expect(main.getByText("Workspace")).toBeVisible({ timeout: 8000 });
    await expect(main.getByText("System", { exact: true })).toBeVisible();
    await expect(main.getByRole("link", { name: "Organization" })).toBeVisible();
    await expect(main.getByRole("link", { name: "AI Settings" })).toBeVisible();
    await expect(main.getByRole("link", { name: "Access Control" })).toBeVisible();
    await expect(main.getByRole("link", { name: "System Status" })).toBeVisible();
    await expect(main.getByRole("link", { name: "Audit Log" })).toBeVisible();

    await page.goto("/access");
    await expect(page.getByRole("tab", { name: "Users" })).toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("tab", { name: "Groups" })).toBeVisible();
    await expect(page.getByRole("tab", { name: "Roles" })).toBeVisible();

    for (const [path, role, label] of [
      ["/queries", "link", "New Query"],
      ["/dashboards", "button", "New Dashboard"],
      ["/monitors", "button", "+ New Monitor"],
      ["/data_sources", "button", "+ New data source"],
    ] as const) {
      await page.goto(path);
      await expect(page.getByRole(role, { name: label })).toBeVisible({ timeout: 8000 });
    }
  } finally {
    await ctx.close();
  }
});


test("dest-mgr: lands on /destinations without view_monitor; Destinations tab visible, Monitors tab absent; create works; sidebar has no separate Destinations row", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "dest-mgr", ["manage_destinations"]);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();
  const name = `e2e-dest-mgr-created-${Date.now()}`;

  try {
    // seededUserContext closes its page, so trigger the landing redirect here; expect
    // /destinations.
    await page.goto("/");
    await page.waitForURL(/\/destinations/, { timeout: 8000 });

    const tabStrip = page.getByRole("tablist");
    await expect(tabStrip.getByRole("tab", { name: "Monitors", exact: true })).toHaveCount(0);
    await expect(tabStrip.getByRole("tab", { name: "Destinations", exact: true })).toBeVisible({ timeout: 8000 });

    // .first(): an empty list also renders a same-labelled CTA.
    const newBtn = page.getByRole("button", { name: "+ New Destination" }).first();
    await expect(newBtn).toBeVisible({ timeout: 8000 });
    await newBtn.click();
    const dialog = page.locator("[role=dialog]");
    await dialog.locator('input[placeholder="My Slack channel"]').fill(name);
    await dialog.getByText("Select type").click();
    await page.getByRole("option", { name: "Email" }).click();
    await dialog.locator("input").nth(1).fill("e2e-dest-mgr@example.com");
    await dialog.locator('button:has-text("Create")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("row", { name: new RegExp(name) })).toBeVisible({ timeout: 8000 });

    const sidebar = page.locator("aside nav");
    await expect(sidebar.getByRole("link", { name: "Destinations", exact: true })).toHaveCount(0);
    await expect(sidebar.getByRole("link", { name: "Monitors", exact: true })).toHaveCount(0);
  } finally {
    const rows = await queryRows<{ id: number }>(
      "SELECT id FROM notification_destinations WHERE name = $1",
      [name],
    );
    if (rows[0]) await adminApi.deleteDestination(rows[0].id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});


test("monitor-only (view_monitor + view_destinations): /destinations reachable read-only (no create/edit/delete, no Actions column); Monitors↔Destinations tab switching both ways", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "monitor-only", ["view_monitor", "view_destinations"]);
  const seeded = await adminApi.createDestination({
    name: `e2e-monitor-only-seed-${Date.now()}`,
    type: "email",
    options: { to: "e2e-monitor-only@example.com" },
  });
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto("/destinations");
    await expectNoDeadEnd(page);
    await expect(page.getByText(seeded.name)).toBeVisible({ timeout: 8000 });

    await expect(page.getByRole("button", { name: "+ New Destination" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: `Edit ${seeded.name}` })).toHaveCount(0);
    await expect(page.getByRole("button", { name: `Delete ${seeded.name}` })).toHaveCount(0);
    await expect(page.getByRole("columnheader", { name: "Actions" })).toHaveCount(0);

    const tabStrip = page.getByRole("tablist");
    await tabStrip.getByRole("tab", { name: "Monitors", exact: true }).click();
    await page.waitForURL(/\/monitors/, { timeout: 8000 });
    await page.getByRole("tablist").getByRole("tab", { name: "Destinations", exact: true }).click();
    await page.waitForURL(/\/destinations/, { timeout: 8000 });
  } finally {
    await adminApi.deleteDestination(seeded.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});


test("monitor viewer: condition fields disabled with a Viewing-only notice, but Subscribe and Acknowledge stay usable", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();

  const ds = await adminApi.createDataSource({
    name: `e2e-monitor-viewer-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const query = await adminApi.createQuery({
    data_source_id: ds.id,
    name: `e2e-monitor-viewer-q-${stamp}`,
    query: "SELECT 1 AS count",
  });
  // Non-editors read the published result, so it must exist before publishing; that renders
  // monitor-column-select instead of the free-text fallback.
  await executeAndWait(adminApi, query.id);
  const full = await adminApi.rawFetch<{ working_version: number }>(`/api/queries/${query.id}`);
  await adminApi.rawFetch(`/api/queries/${query.id}/publish`, {
    method: "POST",
    body: JSON.stringify({ expected_version: full.working_version }),
  });

  const user = await seedCustomPermissionUser(adminApi, "monitor-viewer", ["view_monitor", "view_query", "execute_query"]);
  // Reading a result (which feeds the column Select) needs a non-view-only grant plus
  // execute_query.
  await adminApi.rawFetch(`/api/groups/${user.groupId}/data_sources`, {
    method: "POST",
    body: JSON.stringify({ data_source_id: ds.id, view_only: false }),
  });

  const monitorName = `E2E Monitor Viewer ${stamp}`;
  const monitor = await adminApi.createMonitor({
    query_id: query.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });

  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto(`/monitors/${monitor.id}`);
    await expect(page.getByPlaceholder("Monitor name")).toHaveValue(monitorName, { timeout: 8000 });

    await expect(
      page.getByText("You don't have permission to edit this monitor. Viewing only."),
    ).toBeVisible();
    await expect(page.getByPlaceholder("Monitor name")).toBeDisabled();
    await expect(page.getByRole("spinbutton")).toBeDisabled();
    await expect(page.getByTestId("monitor-column-select")).toBeDisabled();
    await expect(page.getByTestId("monitor-operator-select")).toBeDisabled();

    const subscribeBtn = page.getByRole("button", { name: "Subscribe" });
    await expect(subscribeBtn).toBeEnabled();
    await subscribeBtn.click();
    await expect(page.getByText(user.email)).toBeVisible({ timeout: 8000 });

    await queryRows("UPDATE monitors SET state = 'triggered' WHERE id = $1", [monitor.id]);
    await page.reload();

    const ackBtn = page.getByRole("button", { name: "Acknowledge" });
    await expect(ackBtn).toBeEnabled({ timeout: 8000 });
    await ackBtn.click();
    await expect(page.getByText(/Acknowledged by/)).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteMonitor(monitor.id).catch(() => {});
    await adminApi.deleteQuery(query.id).catch(() => {});
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});
