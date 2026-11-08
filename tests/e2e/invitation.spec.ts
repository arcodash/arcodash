/** E2E: invitation accept, password reset, non-admin authz. */
import { test, expect } from "@playwright/test";
import { loginAsAnyLanding, adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { invitationToken } from "./helpers/permissions";

const NEW_PASSWORD = "InviteePass123!";

async function resetToken(userId: number): Promise<string> {
  const rows = await queryRows<{ token: string | null }>(
    "SELECT details->>'reset_token' AS token FROM users WHERE id = $1",
    [userId],
  );
  const token = rows[0]?.token;
  if (!token) throw new Error(`reset_token missing for user ${userId}`);
  return token;
}

test("admin invites a user via Users page → pending user appears in list", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const email = `e2e-invite-${Date.now()}@example.com`;

  try {
    await page.goto("/users");
    await page.click('button:has-text("Invite User")');
    // Scope to the dialog: an unscoped "Send Invitation" match also hits "Resend invitation" rows
    // behind it.
    const dialog = page.getByRole("dialog", { name: "Invite User" });
    await dialog.locator('input[placeholder="Jane Smith"]').fill("E2E Invitee");
    await dialog.locator('input[placeholder="jane@example.com"]').fill(email);
    await dialog.getByRole("button", { name: "Add group…" }).click();
    await page.getByPlaceholder("Search…").fill("default");
    await page.getByLabel(/^default\b/).click();
    await page.keyboard.press("Escape");
    await dialog.getByRole("button", { name: "Send Invitation", exact: true }).click();

    const row = page.locator("tr", { hasText: email });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Invited")).toBeVisible();

    const rows = await queryRows<{ id: number; pending: boolean }>(
      "SELECT id, is_invitation_pending AS pending FROM users WHERE email = $1",
      [email],
    );
    expect(rows[0]?.pending).toBe(true);
    await invitationToken(rows[0].id); // throws if absent
  } finally {
    // Revoke hard-deletes the pending row; deleteUser only disables it, leaving a stale "Resend
    // invitation" row for the next run.
    const rows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [email]);
    if (rows[0]) await adminApi.revokeInvitation(rows[0].id).catch(() => {});
    await ctx.close();
  }
});

test("re-inviting a still-pending email via Users page updates their group, not the old one", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const email = `e2e-reinvite-${Date.now()}@example.com`;

  async function inviteViaDialog(groupName: string) {
    await page.goto("/users");
    await page.click('button:has-text("Invite User")');
    const dialog = page.getByRole("dialog", { name: "Invite User" });
    await dialog.locator('input[placeholder="Jane Smith"]').fill("E2E Reinvite");
    await dialog.locator('input[placeholder="jane@example.com"]').fill(email);
    await dialog.getByRole("button", { name: "Add group…" }).click();
    await page.getByPlaceholder("Search…").fill(groupName);
    await page.getByLabel(new RegExp(`^${groupName}\\b`)).click();
    await page.keyboard.press("Escape");
    // The row already reads "Invited" after a first invite, so wait on the request itself.
    await Promise.all([
      page.waitForResponse(
        (r) => r.url().endsWith("/api/users") && r.request().method() === "POST" && r.ok(),
      ),
      dialog.getByRole("button", { name: "Send Invitation", exact: true }).click(),
    ]);

    const row = page.locator("tr", { hasText: email });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Invited")).toBeVisible();
  }

  try {
    await inviteViaDialog("default");

    const firstRows = await queryRows<{ id: number; groups: number[] }>(
      "SELECT id, groups FROM users WHERE email = $1",
      [email],
    );
    const userId = firstRows[0].id;
    const [{ id: defaultGroupId }] = await queryRows<{ id: number }>(
      "SELECT id FROM groups WHERE name = 'default' AND org_id = (SELECT org_id FROM users WHERE id = $1)",
      [userId],
    );
    expect(firstRows[0].groups).toEqual([defaultGroupId]);

    // Re-inviting a still-pending email is a resend that replaces the groups, not a second user.
    await inviteViaDialog("viewers");

    const [{ id: viewersGroupId }] = await queryRows<{ id: number }>(
      "SELECT id FROM groups WHERE name = 'viewers' AND org_id = (SELECT org_id FROM users WHERE id = $1)",
      [userId],
    );
    const secondRows = await queryRows<{ id: number; groups: number[] }>(
      "SELECT id, groups FROM users WHERE email = $1",
      [email],
    );
    expect(secondRows).toHaveLength(1); // still one user row, not a duplicate
    expect(secondRows[0].id).toBe(userId);
    expect(secondRows[0].groups).toEqual([viewersGroupId]);
    expect(secondRows[0].groups).not.toContain(defaultGroupId);
  } finally {
    const rows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [email]);
    if (rows[0]) await adminApi.revokeInvitation(rows[0].id).catch(() => {});
    await ctx.close();
  }
});

test("accept invitation via UI → email prefilled → set password → signed in directly, no login step", async ({ page }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-accept-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Accept", email, groups: [] });

  try {
    const token = await invitationToken(invited.id);

    await page.goto(`/users/invitation/accept?token=${token}`);

    await expect(page.getByText(email)).toBeVisible({ timeout: 8000 });

    await page.fill("#inv-name", "E2E Accept");
    await page.fill("#inv-pw", NEW_PASSWORD);
    await page.getByRole("button", { name: "Create account" }).click();

    // Accept signs the user in; groups: [] grants no nav-reachable permission, so the landing route
    // is /settings.
    await page.waitForURL(/\/settings/, { timeout: 8000 });
    const session = (await page.context().cookies()).find((c) => c.name === "arcodash_session");
    expect(session).toBeDefined();

    const rows = await queryRows<{ pending: boolean }>(
      "SELECT is_invitation_pending AS pending FROM users WHERE id = $1",
      [invited.id],
    );
    expect(rows[0]?.pending).toBe(false);
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
  }
});

test("accept page with missing token → shows the dead-link terminal state", async ({ page }) => {
  await page.goto("/users/invitation/accept");
  await expect(
    page.getByRole("heading", { name: "This invitation link isn't valid" }),
  ).toBeVisible({ timeout: 8000 });
  await expect(page.getByRole("link", { name: "Go to sign in" })).toBeVisible();
});

test("accept page with invalid token → shows the dead-link terminal state, no form, no raw backend text", async ({ page }) => {
  await page.goto("/users/invitation/accept?token=not-a-real-token");
  const heading = page.getByRole("heading", { name: "This invitation link isn't valid" });
  await expect(heading).toBeVisible({ timeout: 8000 });
  await expect(page.locator("#inv-pw")).toHaveCount(0);
  await expect(page.getByText(/expired/i)).toHaveCount(0);

  // Focus moves to the heading so a screen reader isn't left on a vanished button.
  await expect(heading).toBeFocused();
});

test("accept page with an expired token → shows the expiry-specific terminal state, not the generic dead-link one", async ({ page }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-expired-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Expired", email, groups: [] });

  try {
    const token = await invitationToken(invited.id);

    await queryRows(
      "UPDATE users SET details = jsonb_set(details, '{invitation_token_expires_at}', to_jsonb(extract(epoch from now())::bigint - 3600)) WHERE id = $1",
      [invited.id],
    );

    await page.goto(`/users/invitation/accept?token=${token}`);
    const heading = page.getByRole("heading", { name: "This invitation link has expired" });
    await expect(heading).toBeVisible({ timeout: 8000 });
    await expect(page.locator("#inv-pw")).toHaveCount(0);
    await expect(
      page.getByRole("heading", { name: "This invitation link isn't valid" }),
    ).toHaveCount(0);
    await expect(page.getByRole("link", { name: "Go to sign in" })).toBeVisible();
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
  }
});

test("accept page: a token already consumed at submit time shows the same terminal state", async ({ page }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-double-accept-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Double Accept", email, groups: [] });

  try {
    const token = await invitationToken(invited.id);

    await page.goto(`/users/invitation/accept?token=${token}`);
    await expect(page.getByText(email)).toBeVisible({ timeout: 8000 });
    await page.fill("#inv-name", "E2E Double Accept");
    await page.fill("#inv-pw", NEW_PASSWORD);

    // Consume the token under the open form (a second tab winning the race); accept now returns
    // 400.
    const raceApi = new E2EApiClient();
    await raceApi.acceptInvitation(token, "E2E Double Accept", NEW_PASSWORD);

    await page.getByRole("button", { name: "Create account" }).click();

    await expect(
      page.getByRole("heading", { name: "This invitation link isn't valid" }),
    ).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
  }
});

test("accepting an invite in a tab that still holds another identity's cached dashboards purges that cache first", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const role = await adminApi.createRole({
    name: `e2e-purge-role-${stamp}`,
    permissions: ["view_dashboard"],
  });
  const group = await adminApi.createGroup({ name: `e2e-purge-group-${stamp}` });
  await adminApi.addGroupRole(group.id, role.id);
  const dashboardName = `E2E Purge Check ${stamp}`;
  const dashboard = await adminApi.createDashboard(dashboardName);
  await adminApi.createWidget({
    dashboard_id: dashboard.id,
    visualization_id: null,
    text: "placeholder",
    options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
  });
  // A view_dashboard-only viewer sees the row only if the dashboard is published.
  await adminApi.publishDashboard(dashboard.slug);
  const email = `e2e-purge-${stamp}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Purge Invitee", email, groups: [group.id] });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    // Warm the dashboards cache as admin: caller.can_edit (which gates Delete) is computed for
    // admin.
    await page.goto("/dashboards");
    await expect(page.getByRole("row", { name: new RegExp(dashboardName) })).toBeVisible({
      timeout: 8000,
    });
    await expect(
      page.getByRole("row", { name: new RegExp(dashboardName) }).getByTitle("Delete"),
    ).toBeVisible();

    // Soft-navigate, not page.goto: a full load recreates the QueryClient and would hide a purge
    // bug.
    const token = await invitationToken(invited.id);
    await page.evaluate((url) => {
      window.history.pushState({}, "", url);
      window.dispatchEvent(new PopStateEvent("popstate"));
    }, `/users/invitation/accept?token=${token}`);

    await expect(page.getByText(email)).toBeVisible({ timeout: 8000 });
    await page.fill("#inv-name", "E2E Purge Invitee");
    await page.fill("#inv-pw", NEW_PASSWORD);
    await page.getByRole("button", { name: "Create account" }).click();
    await page.waitForURL((url) => !url.pathname.startsWith("/users/invitation/accept"), {
      timeout: 8000,
    });

    // A leaked purge would serve the admin-scoped cache instead of refetching.
    await page.evaluate(() => {
      window.history.pushState({}, "", "/dashboards");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    await expect(page.getByRole("row", { name: new RegExp(dashboardName) })).toBeVisible({
      timeout: 8000,
    });
    // view_dashboard only: Delete (gated on caller.can_edit) must be gone.
    await expect(
      page.getByRole("row", { name: new RegExp(dashboardName) }).getByTitle("Delete"),
    ).toHaveCount(0);
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
    await adminApi.deleteDashboard(dashboard.slug).catch(() => {});
    await adminApi.deleteGroup(group.id).catch(() => {});
    await adminApi.deleteRole(role.id).catch(() => {});
    await ctx.close();
  }
});

test("forgot password → reset via UI → can log in with new password", async ({ page }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-reset-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Reset", email, groups: [] });

  try {
    const inviteTok = await invitationToken(invited.id);
    const userApi = new E2EApiClient();
    await userApi.acceptInvitation(inviteTok, "E2E Reset", "InitialPass123!");

    await page.goto("/forgot_password");
    await page.fill("#fp-email", email);
    await page.click('button:has-text("Send Reset Link")');
    await expect(page.getByText(/reset link has been sent/i)).toBeVisible({ timeout: 8000 });

    const rstTok = await resetToken(invited.id);

    const newPassword = "ResetPass456!";
    await page.goto(`/reset_password?token=${rstTok}`);
    await expect(page.locator("#rp-confirm")).toHaveCount(0);
    await page.fill("#rp-pw", newPassword);

    await page.getByRole("button", { name: "Show password" }).click();
    await expect(page.locator("#rp-pw")).toHaveAttribute("type", "text");
    await expect(page.locator("#rp-pw")).toHaveValue(newPassword);
    await page.getByRole("button", { name: "Hide password" }).click();
    await expect(page.locator("#rp-pw")).toHaveAttribute("type", "password");

    await page.click('button:has-text("Set Password")');
    await page.getByRole("button", { name: "Continue to Sign In" }).click();
    await page.waitForURL(/\/login/, { timeout: 8000 });

    // groups: [] means login lands on /settings.
    await loginAsAnyLanding(page, email, newPassword);
    await expect(page).toHaveURL(/\/settings/);
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
  }
});

test("non-admin user is denied admin APIs and the groups page", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-nonadmin-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Non-Admin", email, groups: [] });

  try {
    const token = await invitationToken(invited.id);
    const userApi = new E2EApiClient();
    await userApi.acceptInvitation(token, "E2E Non-Admin", NEW_PASSWORD);
    await userApi.login(email, NEW_PASSWORD);

    expect(await userApi.getStatus("/api/groups")).toBe(403);
    expect(await userApi.getStatus("/api/organization")).toBe(403);

    // The page may redirect or show a forbidden state; "Create group" must never appear.
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    // groups: [] lands on /settings; loginAs's hardcoded /queries wait would race the redirect.
    await loginAsAnyLanding(page, email, NEW_PASSWORD);
    await page.goto("/groups");
    await page.waitForTimeout(1500); // let any redirect / error fetch settle
    await expect(page.locator('input[placeholder="New group name"]')).toHaveCount(0);
    await ctx.close();
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
  }
});

test("accept page scrubs the invitation token from the address bar", async ({ page }) => {
  const SECRET = "SUPERSECRETINVITETOKEN123";
  await page.goto(`/users/invitation/accept?token=${SECRET}`);
  await expect.poll(() => page.url()).not.toContain(SECRET);
});
