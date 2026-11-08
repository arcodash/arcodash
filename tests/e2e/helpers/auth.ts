import {
  type Page,
  type Browser,
  type BrowserContext,
  type BrowserContextOptions,
} from "@playwright/test";

const ADMIN_EMAIL = process.env.ADMIN_EMAIL ?? "admin@example.com";
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD ?? "adminpassword";

/** Must match `SESSION_COOKIE_NAME` in apps/backend/src/auth/session.rs. */
export const SESSION_COOKIE_NAME = "arcodash_session";

export async function loginAs(
  page: Page,
  email: string,
  password: string,
): Promise<void> {
  await page.goto("/login");
  await page.fill("#email", email);
  await page.fill("#password", password);
  await page.click('button[type="submit"]');
  await page.waitForURL(/\/queries/, { timeout: 10_000 });
}

/** Log in without assuming /queries is reachable: a permission-limited user is bounced to their own
 * landing page. Resolves once the URL leaves /login. */
export async function loginAsAnyLanding(
  page: Page,
  email: string,
  password: string,
): Promise<void> {
  await page.goto("/login");
  await page.fill("#email", email);
  await page.fill("#password", password);
  await page.click('button[type="submit"]');
  await page.waitForURL((url) => !url.pathname.startsWith("/login"), { timeout: 10_000 });
}

/** New browser context authenticated as admin; `options` pass through to browser.newContext (e.g.
 * timezoneId). */
export async function adminContext(
  browser: Browser,
  options?: BrowserContextOptions,
): Promise<BrowserContext> {
  const ctx = await browser.newContext(options);
  const page = await ctx.newPage();
  await loginAs(page, ADMIN_EMAIL, ADMIN_PASSWORD);
  await page.close();
  return ctx;
}

/** The CSRF header for raw `ctx.request` calls made with a browser context's cookies. */
export async function csrf(ctx: BrowserContext): Promise<Record<string, string>> {
  const c = (await ctx.cookies()).find((k) => k.name === "csrf_token");
  return c ? { "X-CSRF-Token": c.value } : {};
}
