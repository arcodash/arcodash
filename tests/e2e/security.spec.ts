import { test, expect } from "@playwright/test";
import { loginAs, loginAsAnyLanding } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { queryRows } from "./helpers/db";

const ADMIN_EMAIL = process.env.ADMIN_EMAIL ?? "admin@example.com";
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD ?? "adminpassword";
const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";

test.describe("Internal Error Sanitization", () => {
  test("a real server error surfaces a generic message, never the backend's own text", async ({
    request,
  }) => {
    // playwright.config.ts points LDAP_URL at an unreachable address: a real pre-auth 500 whose raw
    // error text (LDAP host, connection refused) must not leak.
    const res = await request.post(`${BACKEND_URL}/api/ldap/login`, {
      data: { username: "someone", password: "whatever" },
    });
    expect(res.status()).toBe(500);
    const body = await res.json();
    expect(body.message).toBe("internal server error");
    expect(body.message).not.toContain("127.0.0.1");
  });
});

test.describe("Auth State Manipulation", () => {
  test("clearing session cookie redirects to login", async ({ page }) => {
    await loginAs(page, ADMIN_EMAIL, ADMIN_PASSWORD);
    await expect(page).toHaveURL(/\/queries/);

    await page.context().clearCookies({ name: "arcodash_session" });

    await page.reload();
    await expect(page).toHaveURL(/\/login/);
  });

  test("fake session cookie is rejected", async ({ page }) => {
    await page.goto("/login");
    await page.context().addCookies([
      {
        name: "arcodash_session",
        value: "fake-session-cookie-value-that-is-invalid",
        domain: "localhost",
        path: "/",
      },
    ]);

    await page.goto("/queries");
    await expect(page).toHaveURL(/\/login/);
  });
});

test.describe("RBAC Flows", () => {
  test("unauthenticated user cannot reach protected API", async ({ request }) => {
    const res = await request.get("/api/users");
    expect(res.status()).toBe(401);
  });

  test("regular user cannot access admin settings", async ({ browser }) => {
    const adminApi = await loginAsAdmin();
    const stamp = Date.now();
    const email = `security-rbac-user-${stamp}@example.com`;
    let userId: number | undefined;

    try {
      const inviteRes = await adminApi.inviteUser({ name: "Security Test User", email });
      userId = inviteRes.id;
      
      const dbRow = await queryRows<{ details: { invitation_token?: string } }>(
        "SELECT details FROM users WHERE id = $1", [userId]
      );
      if (!dbRow[0] || !dbRow[0].details.invitation_token) throw new Error(`User not found for id ${userId}`);
      const token = dbRow[0].details.invitation_token;
      
      const backendUrl = process.env.BACKEND_URL ?? "http://localhost:8080";
      await fetch(`${backendUrl}/api/users/invitation/accept`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ token, name: "Security Test User", password: "testpassword" })
      });

      const ctx = await browser.newContext();
      const page = await ctx.newPage();
      
      await loginAsAnyLanding(page, email, "testpassword");
      
      await page.goto("/access?tab=users");
      
      await expect(page.getByRole("button", { name: "+ Invite User" })).toHaveCount(0);
      
      await ctx.close();
    } finally {
      if (userId) {
        await adminApi.deleteUser(userId).catch(() => {});
      }
    }
  });
});

test.describe("Input Fuzzing", () => {
  test("submitting malicious strings to login form", async ({ page }) => {
    await page.goto("/login");
    
    await page.fill("#email", "<script>alert('xss')</script>");
    await page.fill("#password", "password");
    await page.evaluate(() => document.querySelector("form")?.setAttribute("novalidate", "true"));
    await page.click('button[type="submit"]');
    await expect(page.locator("p.text-destructive").first()).toBeVisible();
    
    await page.fill("#email", "admin' OR '1'='1");
    await page.fill("#password", "password");
    await page.click('button[type="submit"]');
    await expect(page.locator("p.text-destructive").first()).toBeVisible();
    
    await page.fill("#email", "A".repeat(1000) + "@example.com");
    await page.fill("#password", "password");
    await page.click('button[type="submit"]');
    await expect(page.locator("p.text-destructive").first()).toBeVisible();
  });
});
