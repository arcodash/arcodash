/** E2E: delegatable mutations (invite, group member add, role create, data source create, monitor
 * create) each produce a row in the admin Audit Log. Requires: backend :8080, frontend :3000. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

/** Filter the Audit Log by `action` and assert the row for `objectNeedle` (the object's own name).
 * Match on the object cell only: the shared DB holds many old rows per action and strict mode
 * rejects ambiguous matches. */
async function expectAuditRow(
  page: import("@playwright/test").Page,
  action: string,
  objectNeedle: string,
) {
  await page.goto("/admin/audit");
  await page.getByPlaceholder("Filter by action…").fill(action);
  await expect(page.getByRole("row", { name: objectNeedle })).toBeVisible({ timeout: 8000 });
}

test("audit log shows rows for invite / group-member-add / role-create / DS-create / monitor-create", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();

  const inviteEmail = `e2e-audit-invite-${stamp}@example.com`;
  const groupName = `e2e-audit-group-${stamp}`;
  const memberEmail = `e2e-audit-member-${stamp}@example.com`;
  const roleName = `e2e-audit-role-${stamp}`;
  const dsName = `e2e-audit-ds-${stamp}`;

  let invitedId: number | undefined;
  let memberId: number | undefined;
  let groupId: number | undefined;
  let roleId: number | undefined;
  let dsId: number | undefined;
  let queryId: number | undefined;
  let monitorId: number | undefined;

  try {
    const invited = await adminApi.inviteUser({ name: "Audit Invite", email: inviteEmail, groups: [] });
    invitedId = invited.id;
    await expectAuditRow(page, "user.invited", "Audit Invite");

    const member = await adminApi.inviteUser({ name: "Audit Member", email: memberEmail, groups: [] });
    memberId = member.id;
    const group = await adminApi.createGroup({ name: groupName });
    groupId = group.id;
    await adminApi.addGroupMember(group.id, member.id);
    await expectAuditRow(page, "group.member_added", groupName);

    const role = await adminApi.createRole({ name: roleName, permissions: ["view_query"] });
    roleId = role.id;
    await expectAuditRow(page, "role.created", roleName);

    const ds = await adminApi.createDataSource({
      name: dsName,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    dsId = ds.id;
    await expectAuditRow(page, "data_source.created", dsName);

    // Reuse this test's data source; a queryRows id is a string (bigint) and fails data_source_id:
    // i64.
    const query = await adminApi.createQuery({
      data_source_id: ds.id,
      name: `e2e-audit-query-${stamp}`,
      query: "SELECT 1 AS n",
    });
    queryId = query.id;
    // A monitor can only watch a published query.
    const fullQuery = await adminApi.rawFetch<{ working_version: number }>(
      `/api/queries/${query.id}`,
    );
    await adminApi.rawFetch(`/api/queries/${query.id}/publish`, {
      method: "POST",
      body: JSON.stringify({ expected_version: fullQuery.working_version }),
    });
    const monitorName = `e2e-audit-monitor-${stamp}`;
    const monitor = await adminApi.createMonitor({
      query_id: query.id,
      name: monitorName,
      options: { column: "n", op: ">", value: 0 },
    });
    monitorId = monitor.id;
    await expectAuditRow(page, "monitor.created", monitorName);
  } finally {
    if (monitorId) await adminApi.deleteMonitor(monitorId).catch(() => {});
    if (queryId) await adminApi.deleteQuery(queryId).catch(() => {});
    if (dsId) await adminApi.deleteDataSource(dsId).catch(() => {});
    if (roleId) await adminApi.deleteRole(roleId).catch(() => {});
    if (groupId) await adminApi.deleteGroup(groupId).catch(() => {});
    if (memberId) await adminApi.deleteUser(memberId).catch(() => {});
    if (invitedId) await adminApi.deleteUser(invitedId).catch(() => {});
    await ctx.close();
  }
});

/** Guards the cross-origin X-Total-Count header: if CORS doesn't expose it, fetch sees null and the
 * footer sticks at "0 events". Browser-only, so a reqwest test can't catch it. */
test("audit log total count reflects real events, not the cross-origin-hidden-header 0", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const inviteEmail = `e2e-audit-total-${stamp}@example.com`;

  let invitedId: number | undefined;
  try {
    const invited = await adminApi.inviteUser({ name: "Audit Total", email: inviteEmail, groups: [] });
    invitedId = invited.id;

    await page.goto("/admin/audit");
    await page.getByPlaceholder("Filter by action…").fill("user.invited");
    await expect(page.getByRole("row", { name: "Audit Total" })).toBeVisible({ timeout: 8000 });

    await expect(page.getByText(/^0 events$/)).not.toBeVisible();
  } finally {
    if (invitedId) await adminApi.deleteUser(invitedId).catch(() => {});
    await ctx.close();
  }
});
