import { test, expect } from "@playwright/test";
import { loginAs } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { seedBuiltinRoleUser } from "./helpers/permissions";

const ADMIN_EMAIL = process.env.ADMIN_EMAIL ?? "admin@example.com";
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD ?? "adminpassword";

test("valid credentials → redirect to /queries", async ({ page }) => {
  await loginAs(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await expect(page).toHaveURL(/\/queries/);
});

test("invalid password → shows error, no session cookie", async ({ page }) => {
  await page.goto("/login");
  await page.fill("#email", ADMIN_EMAIL);
  await page.fill("#password", "wrong-password-xyz");
  await page.click('button[type="submit"]');

  await expect(page.locator("p.text-destructive")).toBeVisible({ timeout: 5000 });
  await expect(page).toHaveURL(/\/login/);

  const cookies = await page.context().cookies();
  const session = cookies.find((c) => c.name === "arcodash_session");
  expect(session).toBeUndefined();
});

test("unauthenticated visit to protected route → redirect to /login", async ({ page }) => {
  await page.goto("/queries");
  await expect(page).toHaveURL(/\/login/);
});

test("signed-in visit to a bad URL → 404 with the app shell still there", async ({ page }) => {
  await loginAs(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await page.goto("/settings/access");

  await expect(page.getByText("404")).toBeVisible();
  await expect(page.getByRole("navigation")).toBeVisible();
  await expect(page.getByRole("link", { name: "Queries" })).toBeVisible();
});

test("signed-out visit to a bad URL → bare 404, no app shell", async ({ page }) => {
  await page.goto("/settings/access");

  await expect(page.getByText("404")).toBeVisible();
  await expect(page.getByRole("navigation")).not.toBeAttached();
  // AppShell's <header> always renders; its absence (not the sidebar's) pins down "no shell at
  // all".
  await expect(page.getByRole("banner")).not.toBeAttached();
});

test("csrf_token cookie present after login", async ({ page }) => {
  await loginAs(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  const cookies = await page.context().cookies();
  const csrf = cookies.find((c) => c.name === "csrf_token");
  expect(csrf).toBeDefined();
  expect(csrf!.value.length).toBeGreaterThan(0);
});

// Logout revokes every session of the user, so these sign out as a seeded admin, never the shared
// one.
test("logout → redirects to /login, protected routes redirect again", async ({ page }) => {
  const user = await seedBuiltinRoleUser(await loginAsAdmin(), "logout", "Admin");
  try {
    await loginAs(page, user.email, user.password);
    // Logout lives behind the account dropdown in AppShell.tsx, not a top-level button.
    await page.click(`button:has-text("${user.email}")`);
    await page.click('[role="menuitem"]:has-text("Sign out")');
    await expect(page).toHaveURL(/\/login/, { timeout: 5000 });

    const cookies = await page.context().cookies();
    const session = cookies.find((c) => c.name === "arcodash_session");
    expect(!session || session.value === "").toBeTruthy();

    await page.goto("/queries");
    await expect(page).toHaveURL(/\/login/);
  } finally {
    await user.cleanup();
  }
});

test("sign out from a deep route, sign back in → lands on the landing page, not the pre-sign-out route", async ({
  page,
}) => {
  const user = await seedBuiltinRoleUser(await loginAsAdmin(), "signout-deep", "Admin");
  try {
    await loginAs(page, user.email, user.password);
    await page.goto("/account");
    await expect(page).toHaveURL(/\/account/);

    await page.click(`button:has-text("${user.email}")`);
    await page.click('[role="menuitem"]:has-text("Sign out")');
    await expect(page).toHaveURL(/\/login/, { timeout: 5000 });

    await page.fill("#email", user.email);
    await page.fill("#password", user.password);
    await page.click('button[type="submit"]');

    // An explicit sign-out must not leave a `from: /account` redirect behind (RequireAuth can
    // race logout()'s cache clear), or the next sign-in returns to the page signed out from.
    await expect(page).toHaveURL(/\/queries/, { timeout: 10_000 });
  } finally {
    await user.cleanup();
  }
});
