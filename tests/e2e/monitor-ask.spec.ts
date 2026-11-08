/**
 * E2E: "Add with AI" on the Monitors page. The AI hop (`POST /api/monitors/ask`) is faked with a
 * canned `text/event-stream` body; everything downstream is real: the draft form, then the
 * create-query / publish / create-monitor / subscribe chain against the backend and database.
 */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { getQuery, queryRows } from "./helpers/db";
import { sse } from "./helpers/ai-stub";

const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache" };

async function enableAi(page: Page) {
  await page.route("**/api/session", async (route) => {
    const res = await route.fetch();
    const json = await res.json();
    json.client_config = { ...(json.client_config ?? {}), ai_sql_generation: true, ai_monitor_suggestions: true };
    await route.fulfill({ json });
  });
}

async function withDataSource(fn: (dsId: number) => Promise<void>) {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-monitor-ask-ds-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  try {
    await fn(ds.id);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

test("ask drafts a monitor; Save creates one published query, one baseline monitor, one self-subscription", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);
  const stamp = Date.now();
  const prompt = `watch signups ${stamp}`;
  const monitorName = `Signups baseline ${stamp}`;
  const sql = "SELECT count(*) AS signups FROM orders";

  await withDataSource(async (dsId) => {
    await page.route("**/api/monitors/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["draft", { attempt: 1, sql, explanation: null }],
          ["executing", { attempt: 1 }],
          [
            "preview",
            { attempt: 1, columns: [{ name: "signups", friendly_type: "integer" }], rows: [[3]], row_count: 1, truncated: false },
          ],
          [
            "final",
            { sql, explanation: "Counts signups.", served_by: "Gemini", attempts: 1, tokens_in: 10, tokens_out: 5, executed: true, error: null },
          ],
          ["suggesting", {}],
          [
            "suggested",
            {
              sql,
              data_source_id: dsId,
              suggestion: {
                name: monitorName,
                kind: "baseline",
                column: "signups",
                sensitivity: "medium",
                op: null,
                value: null,
                definition: "Alerts when signups fall outside their usual range.",
                why: "A drop in signups is the earliest sign of a broken funnel.",
                confidence: "high",
              },
            },
          ],
        ]),
      }),
    );

    await page.goto("/monitors");
    await page.getByTestId("monitor-ask-open").click();
    const dialog = page.getByTestId("monitor-ask-dialog");
    await dialog.getByTestId("monitor-ask-prompt").fill(prompt);
    await dialog.getByTestId("monitor-ask-submit").click();

    await expect(page).toHaveURL(/\/monitors\/new/, { timeout: 10_000 });
    await expect(page.getByTestId("monitor-ai-banner")).toBeVisible();
    await expect(page.getByTestId("monitor-draft-sql")).toContainText(sql);
    await expect(page.getByTestId("monitor-draft-query-name")).toHaveValue(prompt);

    await page.getByRole("button", { name: "Save", exact: true }).click();
    await expect(page).toHaveURL(/\/monitors\/\d+$/, { timeout: 15_000 });

    const query = await getQuery(prompt);
    expect(query).not.toBeNull();
    expect(query!.query).toBe(sql);
    expect(query!.published).toBe(true);
    expect(await queryRows("SELECT id FROM queries WHERE name = $1", [prompt])).toHaveLength(1);

    const monitors = await queryRows<{ id: string; options: Record<string, unknown> }>(
      "SELECT id, options FROM monitors WHERE query_id = $1",
      [query!.id],
    );
    expect(monitors).toHaveLength(1);
    expect(monitors[0].options.kind).toBe("baseline");
    expect(monitors[0].options.column).toBe("signups");

    const subs = await queryRows("SELECT id FROM monitor_subscriptions WHERE monitor_id = $1", [Number(monitors[0].id)]);
    expect(subs).toHaveLength(1);
  });
  await ctx.close();
});

test("failure shows the reason; Open in editor prefills the Ask tab", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await enableAi(page);
  const prompt = `watch nothing ${Date.now()}`;

  await withDataSource(async () => {
    await page.route("**/api/monitors/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["failed", { status: "generation_failed", message: "Couldn't find a column to watch" }],
        ]),
      }),
    );

    await page.goto("/monitors");
    await page.getByTestId("monitor-ask-open").click();
    const dialog = page.getByTestId("monitor-ask-dialog");
    await dialog.getByTestId("monitor-ask-prompt").fill(prompt);
    await dialog.getByTestId("monitor-ask-submit").click();

    await expect(dialog.getByText(/Couldn't find a column to watch/)).toBeVisible({ timeout: 10_000 });
    await expect(dialog.getByRole("button", { name: "Try again" })).toBeVisible();

    await dialog.getByTestId("monitor-ask-open-editor").click();
    await expect(page).toHaveURL(/\/queries\/new/);
    await expect(page.getByLabel("Ask AI a question about your data")).toHaveValue(prompt, { timeout: 10_000 });
  });
  await ctx.close();
});
