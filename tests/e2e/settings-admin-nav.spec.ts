// Requires: backend :8080, frontend :3000.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin } from "./helpers/api";

const ADMIN_EMAIL = process.env.ADMIN_EMAIL ?? "admin@example.com";
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD ?? "adminpassword";


test("settings hub: Workspace + System cards each navigate to their route", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/settings");
  await expect(page.getByRole("heading", { name: "Workspace" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "System" })).toBeVisible();

  const cards: [string, string | RegExp][] = [
    ["Organization", /\/organization$/],
    ["AI Settings", /\/organization\/ai$/],
    ["Access Control", /\/access$/],
    ["System Status", /\/admin\/status$/],
    ["Audit Log", /\/admin\/audit$/],
  ];

  // Scope to the page content; the sidebar shows the same labels.
  const main = page.locator("main");
  for (const [label, urlPattern] of cards) {
    await page.goto("/settings");
    await main.getByRole("link", { name: new RegExp(label) }).click();
    await page.waitForURL(urlPattern, { timeout: 8000 });
  }

  await ctx.close();
});

test("settings hub: AI Settings and System Status cards describe what the page actually does", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/settings");
  await expect(page.getByText("AI providers, models, and usage limits")).toBeVisible();
  await expect(page.getByText("Queue depth and recent job activity")).toBeVisible();

  await ctx.close();
});

test("organization page: the raw JSON settings panel is not shown", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/organization");
  await expect(page.getByRole("heading", { name: "Organization Settings" })).toBeVisible({
    timeout: 8000,
  });
  await expect(page.getByText("Settings (JSON)")).not.toBeVisible();

  await ctx.close();
});


test("AI settings: save dummy Gemini key persists masked; Test key fails gracefully (no real creds)", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const dummyKey = `AIzaSyDUMMY-${Date.now()}-NOTREAL`;

  await page.goto("/organization/ai");
  await expect(page.locator("#gemini-key")).toBeVisible({ timeout: 8000 });

  // The test DB persists across runs, so don't assume no org key exists; assert key_configured
  // flips and the raw key is never echoed.
  await page.fill("#gemini-key", dummyKey);
  await page.click('button:has-text("Save")');
  await expect(page.getByText("Saved.", { exact: false })).toBeVisible({ timeout: 8000 });

  const afterSave = await api.getAiSettings();
  expect(afterSave.key_configured).toBe(true);
  expect(JSON.stringify(afterSave)).not.toContain(dummyKey);

  await page.reload();
  await expect(page.locator("#gemini-key")).toHaveValue("");
  await expect(page.locator("#gemini-key")).toHaveAttribute(
    "placeholder",
    /leave blank to keep current/,
  );

  // A fake key hits real Gemini: expect pending, then a graceful failure, never a green "ok". Scope
  // to the Gemini ProviderCard (one per provider, each with "Test key").
  const geminiCard = page.getByTestId("provider-card-gemini");
  const testBtn = geminiCard.getByRole("button", { name: /^(Test key|Testing…)$/ });
  await testBtn.click();
  await expect(testBtn).toHaveText("Testing…");
  await expect(testBtn).toHaveText("Test key", { timeout: 15_000 });

  // Scope to the Gemini card; ProviderPriorityCard has a similar `p.rounded-md.border`.
  const resultText = geminiCard.locator("p.rounded-md.border");
  await expect(resultText).toBeVisible();
  const className = await resultText.getAttribute("class");
  expect(className ?? "").not.toContain("green-600/30");

  await page.click('button:has-text("Remove key")'); // opens the confirm dialog
  await page.getByRole("button", { name: "Remove key" }).last().click(); // confirm inside dialog
  await expect(geminiCard.getByText(/No API key configured/)).toBeVisible({ timeout: 8000 });

  await ctx.close();
});


test("audit log: user column shows a resolved name, not a raw id", async ({ browser }) => {
  // A fresh login guarantees a row for the admin, named "Admin" (bootstrap.rs).
  const c = new E2EApiClient();
  await c.login(ADMIN_EMAIL, ADMIN_PASSWORD);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/admin/audit");
  // Parallel specs log in other users too, so the newest row isn't necessarily the admin's.
  const adminRow = page.locator("tbody tr").filter({ has: page.getByRole("cell", { name: "Admin", exact: true }) }).first();
  await expect(adminRow).toBeVisible({ timeout: 8000 });
  await expect(adminRow.getByText(/^User #\d+$/)).not.toBeVisible();

  await ctx.close();
});


test("audit log: pagination — Next/Prev move between distinct pages", async ({ browser }) => {
  test.slow(); // 55 sequential real logins, each an Argon2id verify; the default 30s budget is too tight under parallel CI shards.
  // Seed >1 page (page_size=50) worth of "login" events via repeated fresh logins.
  for (let i = 0; i < 55; i++) {
    const c = new E2EApiClient();
    await c.login(ADMIN_EMAIL, ADMIN_PASSWORD);
  }

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/admin/audit");
  await expect(page.getByText("Page 1")).toBeVisible({ timeout: 8000 });

  const nextBtn = page.getByRole("button", { name: "Next →" });
  await expect(nextBtn).toBeEnabled({ timeout: 8000 });
  const prevBtn = page.getByRole("button", { name: "← Prev" });
  await expect(prevBtn).toBeDisabled();

  const page1FirstRow = await page.locator("tbody tr").first().innerText();

  await nextBtn.click();
  await expect(page.getByText("Page 2")).toBeVisible({ timeout: 8000 });
  const page2FirstRow = await page.locator("tbody tr").first().innerText();
  expect(page2FirstRow).not.toBe(page1FirstRow);

  await prevBtn.click();
  await expect(page.getByText("Page 1")).toBeVisible({ timeout: 8000 });
  const backToPage1 = await page.locator("tbody tr").first().innerText();
  expect(backToPage1).toBe(page1FirstRow);

  await ctx.close();
});
