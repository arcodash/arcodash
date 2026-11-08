/** Seed users with an exact permission set: a role, a group carrying only that role, and a user
 * invited into only that group. */
import { type Browser, type BrowserContext } from "@playwright/test";
import { E2EApiClient } from "./api";
import { loginAsAnyLanding } from "./auth";
import { queryRows } from "./db";

const PASSWORD = "E2ePermUser123!";

export async function invitationToken(userId: number): Promise<string> {
  const rows = await queryRows<{ token: string | null }>(
    "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
    [userId],
  );
  const token = rows[0]?.token;
  if (!token) throw new Error(`invitation_token missing for user ${userId}`);
  return token;
}

export interface SeededUser {
  email: string;
  password: string;
  userId: number;
  roleId: number;
  groupId: number;
  cleanup: () => Promise<void>;
}

export async function seedCustomPermissionUser(
  adminApi: E2EApiClient,
  label: string,
  permissions: string[],
): Promise<SeededUser> {
  const stamp = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const roleName = `e2e-${label}-role-${stamp}`;
  const groupName = `e2e-${label}-group-${stamp}`;
  const email = `e2e-${label}-${stamp}@example.com`;

  const role = await adminApi.createRole({ name: roleName, permissions });
  const group = await adminApi.createGroup({ name: groupName });
  await adminApi.addGroupRole(group.id, role.id);
  const invited = await adminApi.inviteUser({ name: `E2E ${label}`, email, groups: [group.id] });
  const token = await invitationToken(invited.id);
  const userApi = new E2EApiClient();
  await userApi.acceptInvitation(token, `E2E ${label}`, PASSWORD);

  return {
    email,
    password: PASSWORD,
    userId: invited.id,
    roleId: role.id,
    groupId: group.id,
    cleanup: async () => {
      await adminApi.deleteUser(invited.id).catch(() => {});
      await adminApi.removeGroupRole(group.id, role.id).catch(() => {});
      await adminApi.deleteGroup(group.id).catch(() => {});
      await adminApi.deleteRole(role.id).catch(() => {});
    },
  };
}

/** Create a group carrying an existing BUILTIN role (by name) and invite a user into only it,
 * avoiding the shared "default" group. */
export async function seedBuiltinRoleUser(
  adminApi: E2EApiClient,
  label: string,
  builtinRoleName: string,
): Promise<SeededUser> {
  const roles = await adminApi.listRoles();
  const builtin = roles.find((r) => r.name === builtinRoleName && r.type === "builtin");
  if (!builtin) throw new Error(`Builtin role "${builtinRoleName}" not found`);

  const stamp = `${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const groupName = `e2e-${label}-group-${stamp}`;
  const email = `e2e-${label}-${stamp}@example.com`;

  const group = await adminApi.createGroup({ name: groupName });
  await adminApi.addGroupRole(group.id, builtin.id);
  const invited = await adminApi.inviteUser({ name: `E2E ${label}`, email, groups: [group.id] });
  const token = await invitationToken(invited.id);
  const userApi = new E2EApiClient();
  await userApi.acceptInvitation(token, `E2E ${label}`, PASSWORD);

  return {
    email,
    password: PASSWORD,
    userId: invited.id,
    roleId: builtin.id,
    groupId: group.id,
    cleanup: async () => {
      await adminApi.deleteUser(invited.id).catch(() => {});
      await adminApi.removeGroupRole(group.id, builtin.id).catch(() => {});
      await adminApi.deleteGroup(group.id).catch(() => {});
      // The builtin role itself is shared org-wide state — never deleted.
    },
  };
}

/** New browser context logged in as `user` via the login form; doesn't assume /queries is reachable
 * (see loginAsAnyLanding). */
export async function seededUserContext(
  browser: Browser,
  user: { email: string; password: string },
): Promise<BrowserContext> {
  const ctx = await browser.newContext();
  const page = await ctx.newPage();
  await loginAsAnyLanding(page, user.email, user.password);
  await page.close();
  return ctx;
}
