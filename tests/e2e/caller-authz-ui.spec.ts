// Requires: backend :8080, frontend :3000.
import { test, expect } from "@playwright/test";
import { loginAsAdmin, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { seedCustomPermissionUser, seededUserContext } from "./helpers/permissions";

test("viewer-role dashboard owner sees no Edit button and no rename pencil", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const user = await seedCustomPermissionUser(adminApi, "dash-owner-noedit", [
    "create_dashboard",
    "view_dashboard",
  ]);
  const userApi = new E2EApiClient();
  await userApi.login(user.email, user.password);

  const name = `E2E Owner NoEdit ${Date.now()}`;
  const dashboard = await userApi.createDashboard(name);
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto(`/dashboard/${dashboard.slug}`);
    await expect(page.getByText(name)).toBeVisible({ timeout: 8000 });

    await expect(page.getByRole("button", { name: "Edit", exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Share", exact: true })).toHaveCount(0);
  } finally {
    await userApi.deleteDashboard(dashboard.slug).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});

test("a share-grant recipient without the org capability sees no Edit button", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const name = `E2E Grant NoCap ${Date.now()}`;
  const dashboard = await adminApi.createDashboard(name);

  const user = await seedCustomPermissionUser(adminApi, "dash-grantee-nocap", ["view_dashboard"]);
  // An Editor grant clears reach, but the capability ceiling still denies edit.
  await adminApi.rawFetch(`/api/dashboards/${dashboard.id}/acl`, {
    method: "POST",
    body: JSON.stringify({ grantee_id: user.userId, access_type: "editor" }),
  });

  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto(`/dashboard/${dashboard.slug}`);
    await expect(page.getByText(name)).toBeVisible({ timeout: 8000 });

    await expect(page.getByRole("button", { name: "Edit", exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name, exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Share", exact: true })).toHaveCount(0);
  } finally {
    await adminApi.deleteDashboard(dashboard.slug).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});

test("a query owner who loses execute_query sees a disabled Execute, a banner, and a list lock icon", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();

  const ds = await adminApi.createDataSource({
    name: `e2e-exec-vis-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  // The user starts able to view/create/run; the admin then swaps the group's role to one without
  // execute_query (keeping view/create + reach), so the query stays visible but can't run.
  const user = await seedCustomPermissionUser(adminApi, "exec-vis-owner", [
    "create_query",
    "view_query",
    "execute_query",
    "view_data_source",
  ]);
  await adminApi.assignGroupDataSource(user.groupId, ds.id);

  const userApi = new E2EApiClient();
  await userApi.login(user.email, user.password);
  const query = await userApi.createQuery({
    data_source_id: ds.id,
    name: `e2e-exec-vis-query-${stamp}`,
    query: "SELECT 1 AS count",
  });

  const noExecRole = await adminApi.createRole({
    name: `e2e-exec-vis-noexec-role-${stamp}`,
    permissions: ["create_query", "view_query", "view_data_source"],
  });
  await adminApi.removeGroupRole(user.groupId, user.roleId);
  await adminApi.addGroupRole(user.groupId, noExecRole.id);

  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    // Execute is disabled with tooltip + banner; data-source execute reach also requires
    // execute_query.
    await page.goto(`/queries/${query.id}`);
    const execBtn = page.getByRole("button", { name: "Execute" });
    await expect(execBtn).toBeVisible({ timeout: 8000 });
    await expect(execBtn).toBeDisabled();
    await expect(execBtn).toHaveAttribute(
      "title",
      "You have limited access to this query and its data source, so it can't run. Ask an admin to check your roles and permissions.",
    );
    await expect(
      page.getByText(
        "You have limited access to this query and its data source, so it can't run. Ask an admin to check your roles and permissions.",
      ),
    ).toBeVisible({ timeout: 8000 });

    // Scope to this query's own row; concurrent specs leave their own locked rows (strict-mode
    // ambiguity).
    await page.goto("/queries");
    const row = page.locator("tr", { hasText: `e2e-exec-vis-query-${stamp}` });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(
      row.getByTitle("Limited access — some actions unavailable. Ask an admin."),
    ).toBeVisible({ timeout: 8000 });
  } finally {
    await userApi.deleteQuery(query.id).catch(() => {});
    await adminApi.removeGroupRole(user.groupId, noExecRole.id).catch(() => {});
    await adminApi.deleteRole(noExecRole.id).catch(() => {});
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});

test("a monitor owner without edit_monitor sees no Save/Delete", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();

  // Admin owns the data source + published query; the user needs only View reach to create a
  // monitor, which they own but can't edit.
  const ds = await adminApi.createDataSource({
    name: `e2e-caller-ui-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const query = await adminApi.createQuery({
    data_source_id: ds.id,
    name: `e2e-caller-ui-query-${stamp}`,
    query: "SELECT 1 AS count",
  });
  await adminApi.rawFetch(`/api/queries/${query.id}/publish`, {
    method: "POST",
    body: JSON.stringify({ expected_version: 0 }),
  });

  const user = await seedCustomPermissionUser(adminApi, "mon-owner-noedit", [
    "create_monitor",
    "view_monitor",
    "view_query",
  ]);
  await adminApi.rawFetch(`/api/groups/${user.groupId}/data_sources`, {
    method: "POST",
    body: JSON.stringify({ data_source_id: ds.id, view_only: true }),
  });

  const userApi = new E2EApiClient();
  await userApi.login(user.email, user.password);
  const monitorName = `E2E Owner NoEdit Monitor ${stamp}`;
  const monitor = await userApi.createMonitor({
    query_id: query.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });

  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();

  try {
    await page.goto(`/monitors/${monitor.id}`);
    await expect(page.getByPlaceholder("Monitor name")).toHaveValue(monitorName, { timeout: 8000 });

    await expect(page.getByRole("button", { name: /^save$/i })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^delete$/i })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Share", exact: true })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^mute$/i })).toHaveCount(0);
  } finally {
    await userApi.deleteMonitor(monitor.id).catch(() => {});
    await adminApi.deleteQuery(query.id).catch(() => {});
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await user.cleanup();
    await ctx.close();
  }
});
