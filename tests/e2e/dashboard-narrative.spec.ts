/** E2E: "Explain dashboard" against a local OpenAI-compatible stub that records every prompt, so we
 * can assert which values reach the model. Requires backend :8080, frontend :3000, worker, target
 * postgres :5434; ARCODASH_ALLOW_PRIVATE_CONNECTORS=1 (playwright.config.ts) lets base_url point
 * at localhost. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { seedCustomPermissionUser } from "./helpers/permissions";
import { startStub, useStubProvider, STUB_SUMMARY as SUMMARY } from "./helpers/ai-stub";

test.use({ trace: "off", screenshot: "off" });

test("Explain dashboard: summary panel, body widget link, previous-vs-current prompt, no new runs, no public/no-access leak", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(120_000);
  const api = await loginAsAdmin();
  const stub = await startStub();
  const restore = await useStubProvider(api, stub.baseUrl);
  const stamp = Date.now();
  const cleanups: (() => Promise<unknown>)[] = [];
  try {
    const ds = await api.createDataSource({
      name: `e2e-narr-ds-${stamp}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    cleanups.push(() => api.deleteDataSource(ds.id));
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-narr-q-${stamp}`,
      query: "SELECT 4242 AS orders_total",
    });
    cleanups.push(() => api.deleteQuery(q.id));
    await api.publishQuery(q.id);
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `e2e-narr-viz-${stamp}`,
      options: {},
    });
    cleanups.push(() => api.deleteVisualization(viz.id));
    await executeAndWait(api, q.id);
    const dash = await api.createDashboard(`e2e-narr-dash-${stamp}`);
    cleanups.push(() => api.deleteDashboard(dash.slug));
    cleanups.push(() => api.revokeShareDashboard(dash.slug));
    const widget = await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: viz.id,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    stub.widgetId = widget.id;
    stub.widgetTitle = viz.name;
    const outsider = await seedCustomPermissionUser(api, "narr-none", []);
    cleanups.push(() => outsider.cleanup());
    const ctx = await adminContext(browser);
    cleanups.push(() => ctx.close());
    const page = await ctx.newPage();
    await page.goto(`/dashboard/${dash.slug}`);
    await expect(page.locator("td").filter({ hasText: /^4,?242$/ })).toBeVisible({ timeout: 15_000 });

    const countRuns = async () =>
      Number((await queryRows<{ n: string }>("SELECT count(*) AS n FROM runs WHERE data_source_id = $1", [ds.id]))[0].n);
    const countResults = async () =>
      Number((await queryRows<{ n: string }>("SELECT count(*) AS n FROM query_results WHERE data_source_id = $1", [ds.id]))[0].n);
    const runsBefore = await countRuns();
    const resultsBefore = await countResults();

    await page.getByRole("button", { name: "Explain" }).click();
    const panel = page.locator("section[aria-labelledby=dash-narrative-heading]");
    await expect(panel.getByText(/Orders total is/)).toBeVisible({ timeout: 30_000 });
    expect(stub.prompts).toHaveLength(1);
    expect(stub.prompts[0]).toContain("4242");

    await expect(panel.getByRole("button", { name: /^Show widget/ })).toHaveCount(0);
    await panel.getByRole("button", { name: viz.name }).click();
    await expect(page.locator(`[data-widget-id="${widget.id}"]`)).toBeFocused();

    // Change the cached value in place (no execution), explain again: the prompt carries both.
    const cachedId = (
      await queryRows<{ id: number }>(
        "SELECT COALESCE(published_latest_query_data_id, latest_query_data_id) AS id FROM queries WHERE id = $1",
        [q.id],
      )
    )[0].id;
    const { query_result: cached } = await api.rawFetch<{
      query_result: { data: { columns: unknown[]; rows: unknown[] } };
    }>(`/api/query_results/${cachedId}`);
    await queryRows("UPDATE query_results SET data = $2::jsonb WHERE id = $1", [
      cachedId,
      JSON.stringify({ ...cached.data, rows: [{ orders_total: 7373 }] }),
    ]);
    await page.reload();
    await expect(page.locator("td").filter({ hasText: /^7,?373$/ })).toBeVisible({ timeout: 15_000 });
    await page.getByRole("button", { name: "Explain" }).click();
    await expect.poll(() => stub.prompts.length, { timeout: 30_000 }).toBe(2);
    expect(stub.prompts[1]).toContain("7373");
    expect(stub.prompts[1]).toContain("4242");

    expect(await countRuns()).toBe(runsBefore);
    expect(await countResults()).toBe(resultsBefore);

    await api.publishDashboard(dash.slug);
    const { public_url } = await api.shareDashboard(dash.slug);
    const anon = await browser.newContext();
    const anonPage = await anon.newPage();
    const narrativeCalls: string[] = [];
    anonPage.on("request", (r) => {
      if (r.url().includes("/narrative")) narrativeCalls.push(r.url());
    });
    await anonPage.goto(public_url);
    await expect(anonPage.getByText(dash.name)).toBeVisible({ timeout: 8000 });
    await expect(anonPage.getByText(SUMMARY)).toHaveCount(0);
    await expect(anonPage.getByRole("button", { name: "Explain" })).toHaveCount(0);
    expect(narrativeCalls).toEqual([]);
    expect(await new E2EApiClient().getStatus(`/api/dashboards/${dash.id}/narrative`)).toBe(401);
    await anon.close();

    const other = new E2EApiClient();
    await other.login(outsider.email, outsider.password);
    expect(await other.getStatus(`/api/dashboards/${dash.id}/narrative`)).toBe(404);
    await expect(
      other.rawFetch(`/api/dashboards/${dash.id}/narrative`, { method: "POST" }),
    ).rejects.toThrow(/→ 404/);
  } finally {
    for (const c of cleanups.reverse()) await c().catch(() => {});
    await restore();
    await stub.close();
  }
});
