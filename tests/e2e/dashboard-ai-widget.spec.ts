/**
 * E2E: "Add with AI" on a dashboard in edit mode. The AI hop (`POST /api/dashboards/:id/ai_widget`)
 * is faked with canned `text/event-stream` bodies; the success run backs the faked `created` event
 * with a real query + visualization + draft widget so the tile truly exists.
 */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, type E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { seedBuiltinRoleUser, seededUserContext } from "./helpers/permissions";
import { sse } from "./helpers/ai-stub";

const PROMPT = "the answer to everything";
const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache" };

const previewEvent: [string, Record<string, unknown>] = [
  "preview",
  { attempt: 1, columns: [{ name: "val", friendly_type: "number" }], rows: [[42]], row_count: 1, truncated: false },
];

const finalEvent = (over: Record<string, unknown> = {}): [string, Record<string, unknown>] => [
  "final",
  {
    sql: "SELECT 42 AS val",
    explanation: "The answer.",
    served_by: "Gemini",
    attempts: 1,
    tokens_in: 10,
    tokens_out: 5,
    executed: true,
    error: null,
    ...over,
  },
];

async function withDashboard(
  api: E2EApiClient,
  fn: (c: { dsId: number; dash: { id: number; slug: string } }) => Promise<void>,
) {
  const ds = await api.createDataSource({
    name: `e2e-ai-widget-ds-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const dash = await api.createDashboard(`e2e-ai-widget-${Date.now()}`);
  try {
    await fn({ dsId: ds.id, dash });
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

async function seedTile(api: E2EApiClient, dsId: number, dashId: number) {
  const q = await api.createQuery({ data_source_id: dsId, name: `e2e-ai-widget-q-${Date.now()}`, query: "SELECT 42 AS val" });
  await api.publishQuery(q.id);
  await executeAndWait(api, q.id);
  const viz = await api.createVisualization({ query_id: q.id, type: "table", name: "AI table", options: {} });
  const w = await api.createWidget({ dashboard_id: dashId, visualization_id: viz.id, text: null, options: {} });
  return { queryId: q.id, vizId: viz.id, widgetId: w.id };
}

async function enableAi(page: Page, enabled = true) {
  await page.route("**/api/session", async (route) => {
    const res = await route.fetch();
    const json = await res.json();
    json.client_config = { ...(json.client_config ?? {}), ai_sql_generation: enabled };
    await route.fulfill({ json });
  });
}

test("success: tile lands in the draft, then shows in view mode after publish", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);

  await withDashboard(api, async ({ dsId, dash }) => {
    let queryId: number | undefined;
    let vizId: number | undefined;
    try {
      await page.route(`**/api/dashboards/${dash.id}/ai_widget`, async (route) => {
        const t = await seedTile(api, dsId, dash.id);
        queryId = t.queryId;
        vizId = t.vizId;
        await route.fulfill({
          status: 200,
          headers: sseHeaders,
          body: sse([
            ["status", { text: "Generating SQL" }],
            ["draft", { attempt: 1, sql: "SELECT 42 AS val", explanation: null }],
            ["executing", { attempt: 1 }],
            previewEvent,
            finalEvent(),
            ["visualizing", {}],
            ["created", { query_id: t.queryId, visualization_id: t.vizId, widget_id: t.widgetId, viz_type: "table", row_count: 1 }],
          ]),
        });
      });

      await page.goto(`/dashboard/${dash.slug}`);
      await page.getByRole("button", { name: "Edit", exact: true }).click();
      await page.getByRole("button", { name: "Add with AI" }).first().click();

      const bar = page.getByRole("region", { name: "Add a widget with AI" });
      await bar.getByLabel("Describe the widget you want").fill(PROMPT);
      await bar.getByRole("button", { name: "Add", exact: true }).click();

      await expect(bar).toBeHidden({ timeout: 10_000 });
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });

      await api.publishDashboard(dash.slug);
      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
    } finally {
      if (vizId) await api.deleteVisualization(vizId).catch(() => {});
      if (queryId) await api.deleteQuery(queryId).catch(() => {});
    }
  });
  await ctx.close();
});

test("failure keeps the bar open; Open in editor prefills the Ask tab", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);

  await withDashboard(api, async ({ dash }) => {
    await page.route(`**/api/dashboards/${dash.id}/ai_widget`, (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["failed", { status: "generation_failed", message: "Couldn't add the widget: no valid SQL" }],
        ]),
      }),
    );

    await page.goto(`/dashboard/${dash.slug}`);
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await page.getByRole("button", { name: "Add with AI" }).first().click();

    const bar = page.getByRole("region", { name: "Add a widget with AI" });
    await bar.getByLabel("Describe the widget you want").fill(PROMPT);
    await bar.getByRole("button", { name: "Add", exact: true }).click();

    await expect(bar.getByText(/no valid SQL/)).toBeVisible({ timeout: 10_000 });
    await expect(bar).toBeVisible();

    await bar.getByRole("button", { name: "Open in editor" }).click();
    await expect(page).toHaveURL(/\/queries\/new/);
    await expect(page.getByLabel("Ask AI a question about your data")).toHaveValue(PROMPT, { timeout: 10_000 });
  });
  await ctx.close();
});

test("action is absent when AI is not configured", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page, false);

  await withDashboard(api, async ({ dash }) => {
    await page.goto(`/dashboard/${dash.slug}`);
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await expect(page.getByTestId("actionbar-root").getByRole("button", { name: "Add Widget", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Add with AI" })).toHaveCount(0);
  });
  await ctx.close();
});

test("action is absent for a viewer", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const user = await seedBuiltinRoleUser(adminApi, "ai-widget-viewer", "Viewer");
  const ctx = await seededUserContext(browser, user);
  const page = await ctx.newPage();
  await enableAi(page);

  try {
    await withDashboard(adminApi, async ({ dsId, dash }) => {
      await seedTile(adminApi, dsId, dash.id);
      await adminApi.publishDashboard(dash.slug);
      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.getByText(/e2e-ai-widget-/).first()).toBeVisible({ timeout: 10_000 });
      await expect(page.getByRole("button", { name: "Edit", exact: true })).toHaveCount(0);
      await expect(page.getByRole("button", { name: "Add with AI" })).toHaveCount(0);
    });
  } finally {
    await ctx.close();
    await user.cleanup();
  }
});
