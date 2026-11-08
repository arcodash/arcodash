/** E2E: Account page (name edit, password change) and the login "Forgot password?" link. Uses a
 * dedicated user, never the shared admin, since it changes the password. */
import { test, expect } from "@playwright/test";
import { E2EApiClient, loginAsAdmin } from "./helpers/api";
import { invitationToken } from "./helpers/permissions";

const INITIAL_PASSWORD = "Acct-E2E-Init-1!";
const NEW_PASSWORD = "Acct-E2E-Changed-2!";

/** Invite + accept a fresh user via the API; isolated and safe to mutate. */
async function makeTestUser(
  adminApi: E2EApiClient,
): Promise<{ id: number; email: string }> {
  const email = `e2e-account-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "Account E2E", email });
  const token = await invitationToken(invited.id);
  await adminApi.acceptInvitation(token, "Account E2E", INITIAL_PASSWORD);
  return { id: invited.id, email };
}

test.describe("Personal account page", () => {
  test("edit name → persists, and password change requires the correct current password", async ({ page: p }) => {
    const adminApi = await loginAsAdmin();
    const user = await makeTestUser(adminApi);

    try {
      await p.goto("/login");
      await p.fill("#email", user.email);
      await p.fill("#password", INITIAL_PASSWORD);
      await p.click('button[type="submit"]');
      await p.waitForURL(/\/queries/, { timeout: 10_000 });

      await p.click(`button:has-text("${user.email}")`);
      await p.click('[role="menuitem"]:has-text("Account")');
      await p.waitForURL(/\/account/, { timeout: 5000 });

      await expect(p.locator("#account-email")).toHaveText(user.email);

      const nameInput = p.locator("#account-name");
      await nameInput.fill("Account E2E Renamed");
      await p.click('button:has-text("Save name")');
      await expect(p.getByText("Name updated.")).toBeVisible({ timeout: 5000 });

      await p.reload();
      await expect(p.locator("#account-name")).toHaveValue("Account E2E Renamed");

      await p.fill("#account-current-password", "not-the-real-password");
      await p.fill("#account-new-password", NEW_PASSWORD);
      await p.fill("#account-confirm-password", NEW_PASSWORD);
      await p.click('button:has-text("Change password")');
      await expect(p.getByText("current password is incorrect")).toBeVisible({ timeout: 5000 });

      await p.fill("#account-current-password", INITIAL_PASSWORD);
      await p.fill("#account-new-password", NEW_PASSWORD);
      await p.fill("#account-confirm-password", "does-not-match");
      await p.click('button:has-text("Change password")');
      await expect(p.getByText("New passwords do not match.")).toBeVisible({ timeout: 5000 });

      await p.fill("#account-confirm-password", NEW_PASSWORD);
      await p.click('button:has-text("Change password")');
      await expect(p.getByText("Password changed.")).toBeVisible({ timeout: 5000 });
      await p.reload();
      await expect(p).toHaveURL(/\/account/);

      await p.click(`button:has-text("Account E2E Renamed")`);
      await p.click('[role="menuitem"]:has-text("Sign out")');
      await p.waitForURL(/\/login/, { timeout: 5000 });

      await p.fill("#email", user.email);
      await p.fill("#password", INITIAL_PASSWORD);
      await p.click('button[type="submit"]');
      await expect(p.locator("p.text-destructive")).toBeVisible({ timeout: 5000 });
      await expect(p).toHaveURL(/\/login/);

      await p.fill("#password", NEW_PASSWORD);
      await p.click('button[type="submit"]');
      // Post-login destination is /account (RequireAuth redirects with state), not /queries; only
      // assert login succeeded.
      await p.waitForURL((url) => !url.pathname.startsWith("/login"), { timeout: 10_000 });
    } finally {
      await adminApi.deleteUser(user.id).catch(() => {});
    }
  });
});

test.describe("Login page — forgot password link", () => {
  test("visible on internal auth, navigates to /forgot_password", async ({ page }) => {
    await page.goto("/login");
    const link = page.getByRole("link", { name: "Forgot password?" });
    await expect(link).toBeVisible();
    await link.click();
    await expect(page).toHaveURL(/\/forgot_password/);
  });
});
