// Cap set via the AI Settings UI, spend seeded into ai_usage; POST /api/queries/ask must refuse
// before any provider call.
import { test, expect, type BrowserContext } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows, getUser } from "./helpers/db";
import { startStub, useStubProvider } from "./helpers/ai-stub";

test.use({ trace: "off", screenshot: "off" });

const MODEL = "e2e-budget-model";

test("spend over the cap blocks Ask and shows on the usage page", async ({ browser }) => {
  // Headroom covers waiting on the org AI lock.
  test.setTimeout(90_000);
  const api = await loginAsAdmin();
  const admin = await getUser("admin@example.com");
  expect(admin).not.toBeNull();
  const stub = await startStub();
  const restoreProvider = await useStubProvider(api, stub.baseUrl);
  let ds: { id: number } | undefined;
  let ctx: BrowserContext | undefined;
  try {
    ds = await api.createDataSource({
      name: `e2e-ai-budget-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.route("**/api/session", async (route) => {
      const json = await (await route.fetch()).json();
      json.client_config = { ...(json.client_config ?? {}), ai_sql_generation: true };
      await route.fulfill({ json });
    });

    await page.goto("/organization/ai");
    await page.getByTestId("ai-budget-input").fill("1");
    await page.getByTestId("ai-budget-input").locator("xpath=ancestor::*[contains(@class,'space-y-2')][1]")
      .getByRole("button", { name: "Save", exact: true }).click();
    await expect(page.getByTestId("ai-budget-status")).toHaveText("Current cap: $1.00 per month");

    await queryRows(
      `INSERT INTO ai_usage (org_id, provider, model, feature, status, tokens_in, tokens_out, user_id, cost_usd, created_at)
       VALUES ($1, 'openai', $2, 'ask', 'ok', 1000, 500, $3, 5.00, now())`,
      [admin.org_id, MODEL, admin.id],
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill("how many customers are there");
    await page.getByRole("button", { name: "Generate SQL" }).click();
    await expect(page.getByText(/Monthly AI budget of \$[\d.,]+ reached/).first()).toBeVisible({ timeout: 10_000 });
    expect(stub.prompts).toHaveLength(0);

    await page.goto("/organization/ai/usage");
    const summary = page.getByTestId("ai-budget-summary");
    await expect(summary).toContainText("Spent this month $5.00 of $1.00");
    await expect(page.getByTestId("ai-budget-bar")).toHaveAttribute("aria-valuenow", "100");
    await expect(page.getByTestId("ai-budget-paused")).toBeVisible();

    const byUser = page.getByTestId("ai-usage-by-user");
    await expect(byUser.getByRole("row", { name: new RegExp(`${admin.email}.*\\$5\\.00`) })).toBeVisible();
  } finally {
    await queryRows("DELETE FROM ai_usage WHERE model = $1", [MODEL]).catch(() => {});
    await api.rawFetch("/api/organization/ai", { method: "POST", body: JSON.stringify({ monthly_budget_usd: null }) }).catch(() => {});
    await restoreProvider();
    if (ds) await api.deleteDataSource(ds.id).catch(() => {});
    await stub.close();
    await ctx?.close();
  }
});
