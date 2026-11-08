/** E2E: "Suggest with AI" pane in the query editor's Monitors block. The AI hop (POST
 * /api/queries/:id/suggest_monitors) is faked; Add runs the real create + self-subscribe chain. */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, type E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";

async function enableAi(page: Page) {
  await page.route("**/api/session", async (route) => {
    const res = await route.fetch();
    const json = await res.json();
    json.client_config = {
      ...(json.client_config ?? {}),
      ai_sql_generation: true,
      ai_monitor_suggestions: true,
    };
    await route.fulfill({ json });
  });
}

function suggestion(over: Record<string, unknown>) {
  return {
    name: "Refund rate spike",
    kind: "threshold",
    column: "refund_rate",
    sensitivity: null,
    op: ">",
    value: 5,
    definition: "Alerts when `refund_rate` > 5",
    why: "Refunds above `5%` usually mean a broken checkout.",
    confidence: "high",
    ...over,
  };
}

const okBody = (suggestions: unknown[]) => ({
  status: "ok",
  suggestions,
  already_covered: [],
  unsupported: null,
  served_by: "openai",
});

async function withQuery(
  api: E2EApiClient,
  opts: { publish: boolean },
  fn: (c: { queryId: number }) => Promise<void>,
) {
  const ds = await api.createDataSource({
    name: `e2e-suggest-pane-ds-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-suggest-pane-q-${Date.now()}`,
    query: "SELECT 12 AS refunds, 3.5 AS refund_rate",
  });
  try {
    if (opts.publish) {
      // Publish before executing: results always run the published content.
      await api.publishQuery(q.id);
      await executeAndWait(api, q.id);
    }
    await fn({ queryId: q.id });
  } finally {
    const ms = await queryRows<{ id: string }>("SELECT id FROM monitors WHERE query_id = $1", [q.id]);
    for (const m of ms) await api.deleteMonitor(Number(m.id)).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

test("Add, Skip, Add creates two real monitors and lists them in the Monitors block", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);
  const stamp = Date.now();
  const names = [`Refund rate ${stamp}`, `Skipped ${stamp}`, `Refunds count ${stamp}`];

  await withQuery(api, { publish: true }, async ({ queryId }) => {
    await page.route(`**/api/queries/${queryId}/suggest_monitors`, (route) =>
      route.fulfill({
        json: okBody([
          suggestion({ name: names[0] }),
          suggestion({ name: names[1], column: "refunds", value: 100 }),
          suggestion({ name: names[2], column: "refunds", value: 50, confidence: "low" }),
        ]),
      }),
    );

    await page.goto(`/queries/${queryId}`);
    await page.getByTestId("monitor-suggest-open").click();
    const pane = page.getByTestId("monitor-suggest-pane");
    await expect(page.getByTestId("monitor-suggest-position")).toHaveText("1 of 3", { timeout: 10_000 });
    await expect(page.getByTestId("monitor-suggest-kind")).toHaveText("Threshold");
    await expect(page.getByTestId("monitor-ai-disclosure")).toBeVisible();

    await page.getByTestId("monitor-suggest-add").click();
    await expect(page.getByTestId("monitor-suggest-position")).toHaveText("2 of 3");
    await page.getByTestId("monitor-suggest-skip").click();
    await expect(page.getByTestId("monitor-suggest-position")).toHaveText("3 of 3");
    await expect(pane).toContainText("AI picked this number");
    await page.getByTestId("monitor-suggest-add").click();

    const summary = page.getByTestId("monitor-suggest-summary");
    await expect(summary).toContainText("Added 2 monitors");
    await page.getByTestId("monitor-suggest-done").click();
    await expect(pane).toBeHidden();

    await expect(page.getByRole("link", { name: names[0] })).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("link", { name: names[2] })).toBeVisible();
    await expect(page.getByRole("link", { name: names[1] })).toHaveCount(0);

    const rows = await queryRows<{ id: string; name: string }>(
      "SELECT id, name FROM monitors WHERE query_id = $1",
      [queryId],
    );
    expect(rows.map((r) => r.name).sort()).toEqual([names[0], names[2]].sort());
    for (const r of rows) {
      const subs = await queryRows<{ destination_id: string | null }>(
        "SELECT destination_id FROM monitor_subscriptions WHERE monitor_id = $1",
        [Number(r.id)],
      );
      expect(subs.length).toBe(1);
    }
  });
  await ctx.close();
});

test("free text returns one suggestion shown as 'From your description'", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);

  await withQuery(api, { publish: true }, async ({ queryId }) => {
    const bodies: Record<string, unknown>[] = [];
    await page.route(`**/api/queries/${queryId}/suggest_monitors`, async (route) => {
      const body = route.request().postDataJSON() ?? {};
      bodies.push(body);
      await route.fulfill({
        json: okBody(body.prompt ? [suggestion({ name: "From text" })] : []),
      });
    });

    await page.goto(`/queries/${queryId}`);
    await page.getByTestId("monitor-suggest-open").click();
    await expect(page.getByTestId("monitor-suggest-empty")).toBeVisible({ timeout: 10_000 });

    await page.getByTestId("monitor-suggest-prompt").fill("alert me when refund rate passes 5%");
    await page.getByTestId("monitor-suggest-submit").click();
    await expect(page.getByTestId("monitor-suggest-position")).toHaveText("From your description");
    expect(bodies[0]).toEqual({});
    expect(bodies[1]).toEqual({ prompt: "alert me when refund rate passes 5%" });
  });
  await ctx.close();
});

test("an unpublished query disables the entry with the publish reason", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);

  await withQuery(api, { publish: false }, async ({ queryId }) => {
    await page.goto(`/queries/${queryId}`);
    await expect(page.getByTestId("monitor-suggest-open")).toBeDisabled({ timeout: 10_000 });
    await expect(page.getByTestId("monitor-suggest-disabled-reason")).toContainText(
      "Publish this query to get monitor suggestions — monitors watch the published version.",
    );
  });
  await ctx.close();
});
