/** E2E: access control (/access) — the role -> group -> user grant chain. Requires: backend :8080,
 * frontend :3000. */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, E2EApiClient, executeAndWait, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { seedCustomPermissionUser } from "./helpers/permissions";
import { enqueueEvaluateMonitor, enqueueSendDigest } from "./helpers/valkey";
import { mailpitMessages, pollMailpitFor } from "./helpers/mailpit";

/** Invite `name`/`email` into exactly `groupName` via the Invite User dialog (nothing is
 * pre-selected). */
async function inviteUserIntoOnlyGroup(
  page: Page,
  name: string,
  email: string,
  groupName: string,
) {
  await page.getByRole("button", { name: "+ Invite User" }).click();
  // Named dialog: the closed group-picker Popover also has role="dialog" and lingers in the DOM.
  const dialog = page.getByRole("dialog", { name: "Invite User" });
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.getByPlaceholder("Jane Smith").fill(name);
  await dialog.getByPlaceholder("jane@example.com").fill(email);

  await dialog.getByRole("button", { name: "Add group…" }).click();
  await page.getByPlaceholder("Search…").fill(groupName);
  // The picker is a Popover portalled to <body>, so scope to the page; row names include roles
  // ("default — Editor"), so match a prefix.
  await page.getByLabel(new RegExp(`^${groupName}\\b`)).click();
  await page.keyboard.press("Escape"); // close the popover, not the dialog underneath it
  // Target the chip's remove button; the popover row shares the group text and may be mid-close.
  await expect(dialog.getByRole("button", { name: `Remove ${groupName}` })).toBeVisible({
    timeout: 8000,
  });

  await dialog.locator('button:has-text("Send Invitation")').click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
}

test("role → group → user grants exactly that permission", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const roleName = `E2E Viewer ${stamp}`;
  const groupName = `e2e-access-group-${stamp}`;
  const email = `e2e-access-user-${stamp}@example.com`;

  try {
    await page.goto("/access?tab=roles");
    await page.getByRole("button", { name: "+ New role" }).click();
    const roleDialog = page.locator("[role=dialog]");
    await expect(roleDialog).toBeVisible({ timeout: 8000 });
    await roleDialog.getByLabel("Name", { exact: true }).fill(roleName);
    await roleDialog.getByLabel("View queries", { exact: true }).check();
    await roleDialog.locator('button:has-text("Save")').click();
    await expect(roleDialog).not.toBeVisible({ timeout: 8000 });
    await expect(page.getByRole("row", { name: new RegExp(roleName) })).toBeVisible({
      timeout: 8000,
    });

    await page.goto("/access?tab=groups");
    await page.getByRole("button", { name: "+ New group" }).click();
    const groupCreateDialog = page.locator("[role=dialog]");
    await expect(groupCreateDialog).toBeVisible({ timeout: 8000 });
    await groupCreateDialog.getByPlaceholder("Group name").fill(groupName);
    await groupCreateDialog.locator('button:has-text("Create")').click();
    await expect(groupCreateDialog).not.toBeVisible({ timeout: 8000 });
    const groupRow = page.getByRole("row", { name: new RegExp(groupName) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });

    await groupRow.getByRole("button", { name: "Manage" }).click();
    const manageDialog = page.locator("[role=dialog]");
    await expect(manageDialog).toBeVisible({ timeout: 8000 });
    await manageDialog.getByRole("button", { name: "Attach role…" }).click();
    // The role picker is a portalled Popover (sibling of the dialog); filter by name since roles
    // share substrings.
    await page.getByPlaceholder("Search…").fill(roleName);
    // Attaching round-trips the API before the checkbox reflects it: .click() then wait, not
    // .check().
    await page.getByLabel(roleName, { exact: true }).click();
    await expect(manageDialog.getByText(roleName)).toBeVisible({ timeout: 8000 });
    await page.keyboard.press("Escape"); // close the popover, not the dialog underneath it
    // .first(): the dialog's top-right X is also named "Close".
    await manageDialog.locator('button:has-text("Close")').first().click();
    await expect(manageDialog).not.toBeVisible({ timeout: 8000 });

    await page.goto("/access?tab=users");
    await inviteUserIntoOnlyGroup(page, "E2E Access User", email, groupName);

    const userRow = page.getByRole("row", { name: new RegExp(email) });
    await expect(userRow).toBeVisible({ timeout: 8000 });
    await userRow.getByRole("button", { name: "Edit" }).click();
    const editDialog = page.locator("[role=dialog]");
    await expect(editDialog).toBeVisible({ timeout: 8000 });
    await expect(editDialog.getByText("View queries")).toBeVisible({ timeout: 8000 });
    await expect(editDialog.getByText("Create queries")).toHaveCount(0);
    await expect(editDialog.getByText("Edit queries")).toHaveCount(0);
    await expect(editDialog.getByText("Workspace admin")).toHaveCount(0);
    await editDialog.locator('button:has-text("Cancel")').click();

    // Also assert the granted set is exactly ["view_query"] from the API.
    const userRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      email,
    ]);
    const userId = userRows[0].id;
    const effective = await adminApi.rawFetch<{ permissions: string[]; is_admin: boolean }>(
      `/api/users/${userId}/effective_permissions`,
    );
    expect(effective.permissions).toEqual(["view_query"]);
    expect(effective.is_admin).toBe(false);
  } finally {
    const userRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      email,
    ]);
    if (userRows[0]) await adminApi.deleteUser(userRows[0].id).catch(() => {});

    const groupRows = await queryRows<{ id: number }>("SELECT id FROM groups WHERE name = $1", [
      groupName,
    ]);
    const roleRows = await queryRows<{ id: number }>("SELECT id FROM roles WHERE name = $1", [
      roleName,
    ]);
    // A role can't be deleted while attached; detach first.
    if (groupRows[0] && roleRows[0]) {
      await adminApi
        .rawFetch(`/api/groups/${groupRows[0].id}/roles/${roleRows[0].id}`, { method: "DELETE" })
        .catch(() => {});
    }
    if (groupRows[0]) await adminApi.deleteGroup(groupRows[0].id).catch(() => {});
    if (roleRows[0]) await adminApi.rawFetch(`/api/roles/${roleRows[0].id}`, { method: "DELETE" }).catch(() => {});

    await ctx.close();
  }
});

test("a role holding the admin wildcard reads as 'All permissions', not a bare count of 1", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const roleName = `E2E Wildcard ${Date.now()}`;

  try {
    await page.goto("/access?tab=roles");
    await page.getByRole("button", { name: "+ New role" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await dialog.getByLabel("Name", { exact: true }).fill(roleName);
    await dialog.getByLabel("Administer workspace", { exact: true }).check();
    await dialog.locator('button:has-text("Save")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    const row = page.getByRole("row", { name: new RegExp(roleName) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("All permissions")).toBeVisible();
    await expect(row.getByText("1 permission", { exact: true })).toHaveCount(0);
  } finally {
    const roleRows = await queryRows<{ id: number }>("SELECT id FROM roles WHERE name = $1", [
      roleName,
    ]);
    if (roleRows[0]) await adminApi.rawFetch(`/api/roles/${roleRows[0].id}`, { method: "DELETE" }).catch(() => {});
    await ctx.close();
  }
});

test("a zero-permission role assigned to a group is flagged on the Roles tab", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const roleName = `E2E Empty ${stamp}`;
  const groupName = `e2e-empty-role-group-${stamp}`;

  try {
    await page.goto("/access?tab=roles");
    await page.getByRole("button", { name: "+ New role" }).click();
    const roleDialog = page.locator("[role=dialog]");
    await expect(roleDialog).toBeVisible({ timeout: 8000 });
    await roleDialog.getByLabel("Name", { exact: true }).fill(roleName);
    await roleDialog.locator('button:has-text("Save")').click();
    await expect(roleDialog).not.toBeVisible({ timeout: 8000 });

    const roleRow = page.getByRole("row", { name: new RegExp(roleName) });
    await expect(roleRow).toBeVisible({ timeout: 8000 });
    await expect(
      roleRow.getByLabel("Assigned to a group but grants no permissions"),
    ).toHaveCount(0);

    await page.goto("/access?tab=groups");
    await page.getByRole("button", { name: "+ New group" }).click();
    const groupCreateDialog = page.locator("[role=dialog]");
    await expect(groupCreateDialog).toBeVisible({ timeout: 8000 });
    await groupCreateDialog.getByPlaceholder("Group name").fill(groupName);
    await groupCreateDialog.locator('button:has-text("Create")').click();
    await expect(groupCreateDialog).not.toBeVisible({ timeout: 8000 });
    const groupRow = page.getByRole("row", { name: new RegExp(groupName) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });

    await groupRow.getByRole("button", { name: "Manage" }).click();
    const manageDialog = page.locator("[role=dialog]");
    await expect(manageDialog).toBeVisible({ timeout: 8000 });
    await manageDialog.getByRole("button", { name: "Attach role…" }).click();
    await page.getByPlaceholder("Search…").fill(roleName);
    await page.getByLabel(roleName, { exact: true }).click();
    await expect(manageDialog.getByText(roleName)).toBeVisible({ timeout: 8000 });
    await page.keyboard.press("Escape");
    await manageDialog.locator('button:has-text("Close")').first().click();
    await expect(manageDialog).not.toBeVisible({ timeout: 8000 });

    await page.goto("/access?tab=roles");
    const flaggedRow = page.getByRole("row", { name: new RegExp(roleName) });
    await expect(flaggedRow).toBeVisible({ timeout: 8000 });
    await expect(
      flaggedRow.getByLabel("Assigned to a group but grants no permissions"),
    ).toBeVisible({ timeout: 8000 });
  } finally {
    const groupRows = await queryRows<{ id: number }>("SELECT id FROM groups WHERE name = $1", [
      groupName,
    ]);
    const roleRows = await queryRows<{ id: number }>("SELECT id FROM roles WHERE name = $1", [
      roleName,
    ]);
    if (groupRows[0] && roleRows[0]) {
      await adminApi
        .rawFetch(`/api/groups/${groupRows[0].id}/roles/${roleRows[0].id}`, { method: "DELETE" })
        .catch(() => {});
    }
    if (groupRows[0]) await adminApi.deleteGroup(groupRows[0].id).catch(() => {});
    if (roleRows[0]) await adminApi.rawFetch(`/api/roles/${roleRows[0].id}`, { method: "DELETE" }).catch(() => {});
    await ctx.close();
  }
});

test("group with no role assigned yields empty effective permissions", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const groupName = `e2e-access-norole-group-${stamp}`;
  const email = `e2e-access-norole-user-${stamp}@example.com`;

  try {
    await page.goto("/access?tab=groups");
    await page.getByRole("button", { name: "+ New group" }).click();
    const groupCreateDialog = page.locator("[role=dialog]");
    await expect(groupCreateDialog).toBeVisible({ timeout: 8000 });
    await groupCreateDialog.getByPlaceholder("Group name").fill(groupName);
    await groupCreateDialog.locator('button:has-text("Create")').click();
    await expect(groupCreateDialog).not.toBeVisible({ timeout: 8000 });
    const groupRow = page.getByRole("row", { name: new RegExp(groupName) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });

    await page.goto("/access?tab=users");
    await inviteUserIntoOnlyGroup(page, "E2E No-Role User", email, groupName);

    const userRow = page.getByRole("row", { name: new RegExp(email) });
    await expect(userRow).toBeVisible({ timeout: 8000 });
    await userRow.getByRole("button", { name: "Edit" }).click();
    const editDialog = page.locator("[role=dialog]");
    await expect(editDialog).toBeVisible({ timeout: 8000 });
    await expect(
      editDialog.getByText("No permissions yet. Add this user to a group to grant access."),
    ).toBeVisible({ timeout: 8000 });
    await expect(editDialog.getByText("Workspace admin")).toHaveCount(0);
    await editDialog.locator('button:has-text("Cancel")').click();

    const userRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      email,
    ]);
    const effective = await adminApi.rawFetch<{ permissions: string[]; is_admin: boolean }>(
      `/api/users/${userRows[0].id}/effective_permissions`,
    );
    expect(effective.permissions).toEqual([]);
    expect(effective.is_admin).toBe(false);
  } finally {
    const userRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      email,
    ]);
    if (userRows[0]) await adminApi.deleteUser(userRows[0].id).catch(() => {});
    const groupRows = await queryRows<{ id: number }>("SELECT id FROM groups WHERE name = $1", [
      groupName,
    ]);
    if (groupRows[0]) await adminApi.deleteGroup(groupRows[0].id).catch(() => {});
    await ctx.close();
  }
});

test("adding a data-restricted user to the unrestricted 'viewers' group warns in the Edit dialog and names the restricted group", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const groupName = `e2e-scope-warning-group-${stamp}`;
  const email = `e2e-scope-warning-user-${stamp}@example.com`;

  const restrictedGroup = await adminApi.createGroup({ name: groupName });
  await adminApi.setGroupDataSourceRestricted(restrictedGroup.id, true);
  const invited = await adminApi.inviteUser({
    name: "E2E Scope Warning User",
    email,
    groups: [restrictedGroup.id],
  });

  // Every org seeds a builtin unrestricted "viewers" group; assert it so a violation fails here.
  const groups = await adminApi.listGroups();
  const viewers = groups.find((g) => g.name === "viewers");
  if (!viewers) throw new Error("builtin 'viewers' group not found");
  const viewersAccess = await adminApi.listGroupDataSources(viewers.id);
  if (viewersAccess.restricted) throw new Error("builtin 'viewers' group is unexpectedly restricted");

  try {
    await page.goto("/access?tab=users");
    const userRow = page.getByRole("row", { name: new RegExp(email) });
    await expect(userRow).toBeVisible({ timeout: 8000 });
    await userRow.getByRole("button", { name: "Edit" }).click();
    const dialog = page.getByRole("dialog", { name: "Edit User" });
    await expect(dialog).toBeVisible({ timeout: 8000 });

    await expect(dialog.getByRole("button", { name: /^save$/i })).toBeVisible();
    await expect(dialog.getByText(/data source restriction/)).toHaveCount(0);

    await dialog.getByRole("button", { name: "Add group…" }).click();
    await page.getByPlaceholder("Search…").fill("viewers");
    await page.getByLabel(/^viewers\b/).click();
    await page.keyboard.press("Escape"); // close the popover, not the dialog underneath it

    await expect(
      dialog.getByText(new RegExp(`on ${groupName} no longer applies`)),
    ).toBeVisible({ timeout: 8000 });
    await expect(dialog.getByRole("button", { name: /^save$/i })).toBeEnabled();

    await dialog.locator('button:has-text("Cancel")').click();
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
    await adminApi.deleteGroup(restrictedGroup.id).catch(() => {});
    await ctx.close();
  }
});

test("a rejected data-source grant in the Manage-group dialog surfaces a visible error, not a silent no-op", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const groupName = `e2e-grant-fail-group-${stamp}`;
  const dsName = `e2e-grant-fail-ds-${stamp}`;

  const group = await adminApi.createGroup({ name: groupName });
  const ds = await adminApi.createDataSource({
    name: dsName,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto("/access?tab=groups");
    const groupRow = page.getByRole("row", { name: new RegExp(groupName) });
    await expect(groupRow).toBeVisible({ timeout: 8000 });
    await groupRow.getByRole("button", { name: "Manage" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });

    await expect(dialog.getByRole("switch", { name: "Restricted" })).toBeChecked();
    await expect(dialog.getByText("This group can see and query only the data sources listed below.")).toBeVisible({
      timeout: 8000,
    });

    await page.route(`**/api/groups/${group.id}/data_sources`, (route) => {
      if (route.request().method() !== "POST") return route.continue();
      return route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ message: "Couldn't grant access to this data source right now." }),
      });
    });

    await dialog.getByRole("button", { name: "Add a data source…" }).click();
    await page.getByPlaceholder("Search…").fill(dsName);
    await page.getByRole("button", { name: dsName }).click();

    await expect(
      dialog.getByText("Couldn't grant access to this data source right now."),
    ).toBeVisible({ timeout: 8000 });
    // The rejected grant must not have landed.
    await expect(dialog.getByText(/the list is empty/)).toBeVisible();

    const rows = await queryRows(
      "SELECT 1 FROM data_source_groups WHERE group_id = $1 AND data_source_id = $2",
      [group.id, ds.id],
    );
    expect(rows.length).toBe(0);
  } finally {
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await adminApi.deleteGroup(group.id).catch(() => {});
    await ctx.close();
  }
});

// Revocation: a revoked subscriber gets neither state-change alerts nor the daily digest. Requires:
// backend :8080, worker (redis :6380), target postgres :5434, mailpit :8026/:1026.

/** First enable needs hour+timezone together (set_digest's had_hour gate); UTC keeps send-hour math
 * out. */
async function enableDigest(api: E2EApiClient): Promise<void> {
  await api.rawFetch("/api/users/me/digest", {
    method: "POST",
    body: JSON.stringify({ enabled: true, hour: 8, timezone: "UTC" }),
  });
}

test("revoking a subscriber's monitor access stops the trigger and digest emails, with a still-authorized positive control", async ({}, testInfo) => {
  testInfo.setTimeout(180_000);

  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const monitorName = `e2e-revoke-monitor-${stamp}`;

  // `control` keeps access: it proves an empty Mailpit for `revoked` means "denied", not "never
  // sent".
  const revoked = await seedCustomPermissionUser(adminApi, `revoke-target-${stamp}`, [
    "view_query",
    "view_monitor",
  ]);
  const control = await seedCustomPermissionUser(adminApi, `revoke-control-${stamp}`, [
    "view_query",
    "view_monitor",
  ]);
  // New groups start data-source-restricted; open both up so only monitor-visibility revocation is
  // tested.
  await adminApi.setGroupDataSourceRestricted(revoked.groupId, false);
  await adminApi.setGroupDataSourceRestricted(control.groupId, false);

  const revokedApi = new E2EApiClient();
  await revokedApi.login(revoked.email, revoked.password);
  const controlApi = new E2EApiClient();
  await controlApi.login(control.email, control.password);
  await enableDigest(revokedApi);
  await enableDigest(controlApi);

  const ds = await adminApi.createDataSource({
    name: `e2e-revoke-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await adminApi.createQuery({
    data_source_id: ds.id,
    name: `e2e-revoke-q-${stamp}`,
    // typed_table id=1 has int_col=42 (tests/e2e/seed/target-db.sql).
    query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
  });
  await adminApi.publishQuery(q.id);
  const monitor = await adminApi.createMonitor({
    query_id: q.id,
    name: monitorName,
    options: { column: "count", op: ">", value: 0 },
  });

  try {
    await revokedApi.createMonitorSubscription(monitor.id, { destination_id: null });
    await controlApi.createMonitorSubscription(monitor.id, { destination_id: null });
    const subs = await queryRows<{ user_id: number }>(
      "SELECT user_id FROM monitor_subscriptions WHERE monitor_id = $1",
      [monitor.id],
    );
    // user_id is bigint; node-pg returns a string.
    const subUserIds = subs.map((s) => Number(s.user_id));
    expect(subUserIds).toHaveLength(2);
    expect(subUserIds).toContain(revoked.userId);
    expect(subUserIds).toContain(control.userId);

    await adminApi.rawFetch(`/api/groups/${revoked.groupId}/members/${revoked.userId}`, {
      method: "DELETE",
    });

    await executeAndWait(adminApi, q.id);
    const monitorRows = await queryRows<{ org_id: number }>(
      "SELECT org_id FROM monitors WHERE id = $1",
      [monitor.id],
    );
    const orgId = monitorRows[0].org_id;
    await enqueueEvaluateMonitor(orgId, monitor.id);

    await expect
      .poll(
        async () => {
          const r = await queryRows<{ state: string }>("SELECT state FROM monitors WHERE id = $1", [
            monitor.id,
          ]);
          return r[0]?.state;
        },
        { timeout: 90_000, intervals: [2000] },
      )
      .toBe("triggered");

    // Positive control first: proves mail was produced, so empty mail for the revoked user isn't
    // the send path never running.
    await pollMailpitFor(
      (m) => m.Subject.includes(monitorName) && m.To?.some((t) => t.Address === control.email),
      90_000,
    );
    await new Promise((r) => setTimeout(r, 3000)); // let the revoked subscriber's own (skipped) send settle
    expect(
      (await mailpitMessages()).some(
        (m) => m.Subject.includes(monitorName) && m.To?.some((t) => t.Address === revoked.email),
      ),
    ).toBe(false);

    // Digest path: enqueue SendDigest directly instead of waiting for the hourly fan-out.
    await enqueueSendDigest(orgId, control.userId);
    await pollMailpitFor(
      (m) =>
        m.Subject.includes("arcodash digest") &&
        m.Snippet?.includes(monitorName) &&
        m.To?.some((t) => t.Address === control.email),
      90_000,
    );
    await enqueueSendDigest(orgId, revoked.userId);
    await new Promise((r) => setTimeout(r, 5000));
    expect(
      (await mailpitMessages()).some(
        (m) =>
          m.Subject.includes("arcodash digest") &&
          m.Snippet?.includes(monitorName) &&
          m.To?.some((t) => t.Address === revoked.email),
      ),
    ).toBe(false);
  } finally {
    await adminApi.deleteMonitor(monitor.id).catch(() => {});
    await adminApi.deleteQuery(q.id).catch(() => {});
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    await revoked.cleanup();
    await control.cleanup();
  }
});
