// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";

test.describe.serial("dashboard tile trust affordances", () => {
  const stamp = Date.now();
  const queryName = `Revenue by region ${stamp}`;
  const vizName = `Revenue by region viz ${stamp}`;
  let api: E2EApiClient;
  let dsId: number;
  let createdQrDsId: number | undefined;
  let qrDsName: string;
  let queryId: number;
  let vizId: number;
  let dashSlug: string;
  let dashId: number;
  const monitorIds: number[] = [];

  test.beforeAll(async () => {
    api = await loginAsAdmin();
    const ds = await api.createDataSource({
      name: `e2e-tile-trust-ds-${stamp}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    dsId = ds.id;

    // The explore draft targets the first query_results source the API lists.
    const list = await api.rawFetch<{ data_sources: { id: number; name: string; type: string }[] }>(
      "/api/data_sources",
    );
    let qr = list.data_sources.find((d) => d.type === "query_results");
    if (!qr) {
      const created = await api.createDataSource({
        name: `e2e-tile-trust-qr-${stamp}`,
        type: "query_results",
        options: {},
      });
      createdQrDsId = created.id;
      qr = { id: created.id, name: created.name, type: "query_results" };
    }
    qrDsName = qr.name;

    const q = await api.createQuery({
      data_source_id: dsId,
      name: queryName,
      query: "SELECT 'north' AS region, 10 AS revenue UNION ALL SELECT 'south', 20",
    });
    queryId = q.id;
    await api.publishQuery(queryId);
    await executeAndWait(api, queryId);
    const viz = await api.createVisualization({
      query_id: queryId,
      type: "table",
      name: vizName,
      options: {},
    });
    vizId = viz.id;

    const dash = await api.createDashboard(`e2e-tile-trust-dash-${stamp}`);
    dashSlug = dash.slug;
    dashId = dash.id;
    await api.createWidget({
      dashboard_id: dashId,
      visualization_id: vizId,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
    });
    await api.publishDashboard(dashSlug);
  });

  test.afterAll(async () => {
    for (const id of monitorIds) await api.deleteMonitor(id).catch(() => {});
    await api.deleteDashboard(dashSlug).catch(() => {});
    await api.deleteVisualization(vizId).catch(() => {});
    await api.deleteQuery(queryId).catch(() => {});
    if (createdQrDsId) await api.deleteDataSource(createdQrDsId).catch(() => {});
    await api.deleteDataSource(dsId).catch(() => {});
  });

  test("tile shows age and empty check state", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.goto(`/dashboard/${dashSlug}`);
    const footer = page.getByTestId("tile-footer");
    await expect(footer).toBeVisible({ timeout: 15_000 });
    await expect(footer).toContainText("Updated");
    await expect(footer).toContainText("No checks · add one");
    await ctx.close();
  });

  test("bell opens a prefilled monitor form", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const monitorName = `tile-trust-monitor-${stamp}`;
    await page.goto(`/dashboard/${dashSlug}`);
    await page.getByRole("link", { name: `Add a monitor for ${vizName}` }).click();
    await expect(page).toHaveURL(new RegExp(`/monitors/new\\?query_id=${queryId}$`));
    await expect(page.getByTestId("monitor-query-select")).toContainText(queryName, {
      timeout: 10_000,
    });

    await page.fill('input[placeholder="Monitor name"]', monitorName);
    await page.getByTestId("monitor-column-select").click();
    await page.getByRole("option", { name: "revenue" }).click();
    await page.click('button:has-text("Save")');
    await page.waitForURL(/\/monitors\/\d+$/, { timeout: 10_000 });

    const rows = await queryRows<{ id: number }>("SELECT id FROM monitors WHERE name = $1", [
      monitorName,
    ]);
    for (const r of rows) monitorIds.push(Number(r.id));

    await page.goto(`/dashboard/${dashSlug}`);
    await expect(page.getByTestId("tile-footer")).toContainText(/1 check (pending|passes)/, {
      timeout: 15_000,
    });
    await ctx.close();
  });

  test("explore this tile opens a draft over query results", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.goto(`/dashboard/${dashSlug}`);
    await page.getByRole("button", { name: `Tile menu for ${vizName}` }).click();
    await page.getByRole("menuitem", { name: "Explore this tile" }).click();

    await expect(page).toHaveURL(/\/queries\/new/);
    await expect(page.locator(".cm-content")).toContainText(`FROM query_${queryId}`, {
      timeout: 10_000,
    });
    await expect(page.locator("#query-name")).toHaveValue(`Explore: ${queryName}`);
    await expect(page.getByTestId("query-editor-data-source-select")).toContainText(qrDsName);

    await page.click('button:has-text("Execute")');
    await expect(page.locator("td").filter({ hasText: /^(north|south)$/ }).first()).toBeVisible({
      timeout: 30_000,
    });
    await ctx.close();
  });
});
