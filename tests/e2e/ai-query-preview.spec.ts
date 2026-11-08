import { test, expect } from "@playwright/test";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

const ADMIN_EMAIL = process.env.ADMIN_EMAIL ?? "admin@example.com";
const ADMIN_PASSWORD = process.env.ADMIN_PASSWORD ?? "adminpassword";

function askBody(sql: string): string {
  const events: [string, Record<string, unknown>][] = [
    ["status", { text: "Generating SQL" }],
    ["draft", { attempt: 1, sql, explanation: null }],
    ["executing", { attempt: 1 }],
    [
      "preview",
      {
        attempt: 1,
        columns: [{ name: "id", friendly_type: "number" }],
        rows: [[1]],
        row_count: 1,
        truncated: false,
      },
    ],
    ["final", { sql, explanation: "Stubbed.", served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1 }],
  ];
  return events.map(([name, data]) => `event: ${name}\ndata: ${JSON.stringify(data)}\n\n`).join("");
}

const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache" };

test("generated SQL preview wraps long statements instead of scrolling horizontally", async ({ page }) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-ai-sql-wraps-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const longSql =
    "SELECT id, name, email, created_at, updated_at, status, country, plan, referral_source " +
    "FROM users WHERE status = 'active' AND created_at > now() - interval '30 days' ORDER BY created_at DESC";

  try {
    await page.route("**/api/session", async (route) => {
      const res = await route.fetch();
      const json = await res.json();
      json.client_config = { ...(json.client_config ?? {}), ai_sql_generation: true };
      await route.fulfill({ json });
    });
    await page.route("**/api/queries/ask", async (route) =>
      route.fulfill({ status: 200, headers: sseHeaders, body: askBody(longSql) }),
    );

    await page.goto("/login");
    await page.getByLabel(/email/i).fill(ADMIN_EMAIL);
    await page.getByLabel(/password/i).fill(ADMIN_PASSWORD);
    await page.getByRole("button", { name: /^sign in$/i }).click();
    await page.waitForURL((url) => !url.pathname.startsWith("/login"), { timeout: 10_000 });

    await page.goto("/queries/new");
    await page.getByRole("button", { name: /ask with ai/i }).click();
    await page.getByLabel(/ask ai a question about your data/i).fill("recent active users");
    await page.getByRole("button", { name: /^generate sql$/i }).click();

    await expect(page.locator("[data-testid=ask-timeline][data-phase=done]")).toBeVisible({ timeout: 15_000 });
    const preview = page.locator(".cm-editor").first();
    await expect(preview).toBeVisible();
    await expect(preview).toContainText("FROM users");
    await expect(page.getByRole("button", { name: /^apply$/i })).toBeVisible();
    await expect(page.getByRole("button", { name: /^regenerate$/i })).toBeVisible();
    await expect(page.getByRole("button", { name: /^discard$/i })).toBeVisible();

    const overflow = await preview.evaluate((el) => el.scrollWidth - el.clientWidth);
    expect(overflow).toBeLessThanOrEqual(1);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});
