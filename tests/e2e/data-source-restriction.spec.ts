/** E2E: data-source group restriction — the Restricted toggle + allowlist. Requires: backend :8080,
 * frontend :3000. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, executeAndWait, TARGET_DS_OPTIONS } from "./helpers/api";
import { seedCustomPermissionUser, seededUserContext, invitationToken } from "./helpers/permissions";
import { queryRows } from "./helpers/db";
import { typeSql } from "./helpers/editor";
import { selectDataSource } from "./helpers/actions";

const PASSWORD = "E2eDsRestrictUser123!";

test("restrict toggle scopes member visibility to the allowlist; unrestricting restores full visibility", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const dsAllowed = await adminApi.createDataSource({
    name: `e2e-ds-restrict-allow-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dsOther = await adminApi.createDataSource({
    name: `e2e-ds-restrict-other-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const user = await seedCustomPermissionUser(adminApi, "ds-restrict-member", ["view_data_source"]);
  const groups = await adminApi.listGroups();
  const group = groups.find((g) => g.id === user.groupId)!;

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const memberCtx = await seededUserContext(browser, user);
  const memberPage = await memberCtx.newPage();

  try {
    await adminPage.goto("/access?tab=groups");
    const groupRow = adminPage.getByRole("row", { name: new RegExp(group.name) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });
    await groupRow.getByRole("button", { name: "Manage" }).click();
    const manageDialog = adminPage.locator("[role=dialog]");
    await expect(manageDialog).toBeVisible({ timeout: 8000 });

    await expect(manageDialog.getByRole("switch")).toBeChecked();
    await expect(
      manageDialog.getByText("This group can see and query only the data sources listed below."),
    ).toBeVisible({ timeout: 8000 });
    await manageDialog.getByRole("button", { name: "Add a data source…" }).click();
    await adminPage.getByPlaceholder("Search…").fill(dsAllowed.name);
    await adminPage.getByRole("button", { name: dsAllowed.name, exact: true }).click();
    await expect(manageDialog.getByText(dsAllowed.name)).toBeVisible({ timeout: 8000 });
    await adminPage.keyboard.press("Escape"); // close the picker popover, not the dialog underneath it
    await manageDialog.locator('button:has-text("Close")').first().click();
    await expect(manageDialog).not.toBeVisible({ timeout: 8000 });

    await memberPage.goto("/data_sources");
    await expect(memberPage.getByText(dsAllowed.name)).toBeVisible({ timeout: 8000 });
    await expect(memberPage.getByText(dsOther.name)).toHaveCount(0);

    await adminPage.goto("/access?tab=groups");
    const groupRowAgain = adminPage.getByRole("row", { name: new RegExp(group.name) });
    await groupRowAgain.getByRole("button", { name: "Manage" }).click();
    const manageDialog2 = adminPage.locator("[role=dialog]");
    await expect(manageDialog2).toBeVisible({ timeout: 8000 });
    // The label flips optimistically; the member's reload must follow the persisted write.
    await Promise.all([
      adminPage.waitForResponse(
        (r) => r.url().includes("/data_source_access") && r.request().method() === "PUT" && r.ok(),
      ),
      manageDialog2.getByRole("switch").click(),
    ]);
    await expect(
      manageDialog2.getByText("This group can see and query all data sources."),
    ).toBeVisible({ timeout: 8000 });
    await manageDialog2.locator('button:has-text("Close")').first().click();
    await expect(manageDialog2).not.toBeVisible({ timeout: 8000 });

    await Promise.all([
      memberPage.waitForResponse((r) => /\/api\/data_sources(\?|$)/.test(r.url()) && r.ok()),
      memberPage.reload(),
    ]);
    await expect(memberPage.getByText(dsAllowed.name)).toBeVisible({ timeout: 8000 });
    await expect(memberPage.getByText(dsOther.name)).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteDataSource(dsAllowed.id).catch(() => {});
    await adminApi.deleteDataSource(dsOther.id).catch(() => {});
    await user.cleanup();
    await adminCtx.close();
    await memberCtx.close();
  }
});

test("member in two restricted groups sees the union of both allowlists", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const dsA = await adminApi.createDataSource({
    name: `e2e-ds-union-a-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dsB = await adminApi.createDataSource({
    name: `e2e-ds-union-b-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dsNeither = await adminApi.createDataSource({
    name: `e2e-ds-union-neither-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  const role = await adminApi.createRole({
    name: `e2e-ds-union-role-${stamp}`,
    permissions: ["view_data_source"],
  });
  const groupA = await adminApi.createGroup({ name: `e2e-ds-union-groupA-${stamp}` });
  const groupB = await adminApi.createGroup({ name: `e2e-ds-union-groupB-${stamp}` });
  await adminApi.addGroupRole(groupA.id, role.id);
  await adminApi.addGroupRole(groupB.id, role.id);
  await adminApi.setGroupDataSourceRestricted(groupA.id, true);
  await adminApi.setGroupDataSourceRestricted(groupB.id, true);
  await adminApi.assignGroupDataSource(groupA.id, dsA.id);
  await adminApi.assignGroupDataSource(groupB.id, dsB.id);

  const email = `e2e-ds-union-member-${stamp}@example.com`;
  const invited = await adminApi.inviteUser({
    name: "E2E Union Member",
    email,
    groups: [groupA.id, groupB.id],
  });
  const token = await invitationToken(invited.id);
  const memberApi = new E2EApiClient();
  await memberApi.acceptInvitation(token, "E2E Union Member", PASSWORD);

  const ctx = await seededUserContext(browser, { email, password: PASSWORD });
  const page = await ctx.newPage();

  try {
    await page.goto("/data_sources");
    await expect(page.getByText(dsA.name)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(dsB.name)).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(dsNeither.name)).toHaveCount(0);
  } finally {
    await adminApi.deleteDataSource(dsA.id).catch(() => {});
    await adminApi.deleteDataSource(dsB.id).catch(() => {});
    await adminApi.deleteDataSource(dsNeither.id).catch(() => {});
    await adminApi.deleteUser(invited.id).catch(() => {});
    await adminApi.removeGroupRole(groupA.id, role.id).catch(() => {});
    await adminApi.removeGroupRole(groupB.id, role.id).catch(() => {});
    await adminApi.deleteGroup(groupA.id).catch(() => {});
    await adminApi.deleteGroup(groupB.id).catch(() => {});
    await adminApi.deleteRole(role.id).catch(() => {});
    await ctx.close();
  }
});

test("create is blocked when every group is restricted; allowed when any group is unrestricted", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const seedDs = await adminApi.createDataSource({
    name: `e2e-ds-createblock-seed-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  // manage_data_source implies view/create/edit_data_source (permissions_catalog.rs), so one role
  // covers both cases.
  const blocked = await seedCustomPermissionUser(adminApi, "ds-blocked", ["manage_data_source"]);
  await adminApi.setGroupDataSourceRestricted(blocked.groupId, true);
  await adminApi.assignGroupDataSource(blocked.groupId, seedDs.id);

  // Same capability, but its only group stays unrestricted — the union rule allows create.
  const allowed = await seedCustomPermissionUser(adminApi, "ds-allowed", ["manage_data_source"]);
  await adminApi.setGroupDataSourceRestricted(allowed.groupId, false);

  const blockedCtx = await seededUserContext(browser, blocked);
  const blockedPage = await blockedCtx.newPage();
  const allowedCtx = await seededUserContext(browser, allowed);
  const allowedPage = await allowedCtx.newPage();
  const dsName = `e2e-ds-createblock-new-${stamp}`;

  try {
    // The disabled state sets an aria-label overriding the accessible name; match on text, not role
    // name.
    await blockedPage.goto("/data_sources");
    const blockedBtn = blockedPage.locator("button", { hasText: "+ New data source" });
    await expect(blockedBtn).toBeVisible({ timeout: 8000 });
    await expect(blockedBtn).toBeDisabled();
    await expect(blockedBtn).toHaveAttribute(
      "title",
      "Your access is limited to specific data sources — ask an admin to add more.",
    );
    await expect(
      blockedPage.getByText(
        "You can't add a data source right now because your access is limited to specific data sources. Ask a workspace admin to widen your access.",
      ),
    ).toBeVisible({ timeout: 8000 });

    await allowedPage.goto("/data_sources");
    const allowedBtn = allowedPage.getByRole("button", { name: "+ New data source" });
    await expect(allowedBtn).toBeVisible({ timeout: 8000 });
    await expect(allowedBtn).toBeEnabled();
    await expect(
      allowedPage.getByText("You can't add a data source right now", { exact: false }),
    ).toHaveCount(0);

    await allowedBtn.click();
    const wizard = allowedPage.locator("[role=dialog]");
    await expect(wizard).toBeVisible({ timeout: 8000 });
    await wizard.getByText("PostgreSQL", { exact: true }).click();
    await expect(wizard.locator("#ds-name")).toBeVisible({ timeout: 8000 });
    await wizard.locator("#ds-name").fill(dsName);
    await wizard.locator('input[id*="host"]').fill(TARGET_DS_OPTIONS.host);
    await wizard.locator('input[id*="port"]').fill(String(TARGET_DS_OPTIONS.port));
    await wizard.locator('input[id*="dbname"]').fill(TARGET_DS_OPTIONS.dbname);
    await wizard.locator('input[id*="user"]').fill(TARGET_DS_OPTIONS.user);
    await wizard.locator('input[id*="password"]').fill(TARGET_DS_OPTIONS.password);
    await wizard.locator('button:has-text("Create")').click();
    await allowedPage.waitForURL(/\/data_sources\/\d+/, { timeout: 10_000 });
    await expect(allowedPage.getByText(dsName)).toBeVisible({ timeout: 8000 });
  } finally {
    const rows = await queryRows<{ id: number }>("SELECT id FROM data_sources WHERE name = $1", [
      dsName,
    ]);
    if (rows[0]) await adminApi.deleteDataSource(rows[0].id).catch(() => {});
    await adminApi.deleteDataSource(seedDs.id).catch(() => {});
    await blocked.cleanup();
    await allowed.cleanup();
    await blockedCtx.close();
    await allowedCtx.close();
  }
});

test("query_results connector refuses to serve cached results from a data source outside the caller's restricted allowlist", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();

  const dsAllowed = await adminApi.createDataSource({
    name: `e2e-qr-allowed-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dsDenied = await adminApi.createDataSource({
    name: `e2e-qr-denied-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dsQueryResults = await adminApi.createDataSource({
    name: `e2e-qr-meta-${stamp}`,
    type: "query_results",
    options: {},
  });

  const qAllowed = await adminApi.createQuery({
    data_source_id: dsAllowed.id,
    name: `e2e-qr-allowed-query-${stamp}`,
    query: "SELECT 1 AS a",
  });
  await adminApi.publishQuery(qAllowed.id);
  await executeAndWait(adminApi, qAllowed.id);

  const qDenied = await adminApi.createQuery({
    data_source_id: dsDenied.id,
    name: `e2e-qr-denied-query-${stamp}`,
    query: "SELECT 2 AS b",
  });
  await adminApi.publishQuery(qDenied.id);
  await executeAndWait(adminApi, qDenied.id);

  const user = await seedCustomPermissionUser(adminApi, "qr-restrict-member", [
    "manage_query",
    "view_data_source",
  ]);
  await adminApi.setGroupDataSourceRestricted(user.groupId, true);
  await adminApi.assignGroupDataSource(user.groupId, dsAllowed.id);
  await adminApi.assignGroupDataSource(user.groupId, dsQueryResults.id);

  const memberCtx = await seededUserContext(browser, user);
  const memberPage = await memberCtx.newPage();

  try {
    // Query the denied query's cached rows via the query_results meta-connector through the UI, as
    // an attacker would.
    await memberPage.goto("/queries/new");
    await memberPage.waitForSelector(`[id="query-name"]`, { timeout: 8000 });
    await memberPage.fill("#query-name", `e2e-qr-attack-${stamp}`);
    await selectDataSource(memberPage, dsQueryResults.name);
    await typeSql(memberPage, `SELECT * FROM query_${qDenied.id}`);
    await memberPage.click('button:has-text("Execute")');

    const resultsPane = memberPage.getByTestId("query-results-pane");
    await expect(resultsPane.locator(".text-destructive")).toBeVisible({ timeout: 10_000 });
    await expect(memberPage.locator("table")).toHaveCount(0);

    // Autosave persists the ad-hoc query and swaps the URL onto its id; capture it for cleanup.
    await memberPage.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
    const attackQueryId = Number(memberPage.url().match(/\/queries\/(\d+)/)![1]);

    // An in-scope query must still work, proving the failure above is authorization.
    await typeSql(memberPage, `SELECT * FROM query_${qAllowed.id}`);
    await memberPage.click('button:has-text("Execute")');
    await expect(memberPage.locator("table")).toBeVisible({ timeout: 10_000 });

    await adminApi.deleteQuery(attackQueryId).catch(() => {});
  } finally {
    await adminApi.deleteQuery(qAllowed.id).catch(() => {});
    await adminApi.deleteQuery(qDenied.id).catch(() => {});
    await adminApi.deleteDataSource(dsAllowed.id).catch(() => {});
    await adminApi.deleteDataSource(dsDenied.id).catch(() => {});
    await adminApi.deleteDataSource(dsQueryResults.id).catch(() => {});
    await user.cleanup();
    await memberCtx.close();
  }
});
