// Requires: backend :8080, worker, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";

test.describe.serial("tile baseline monitor and new-results pill", () => {
  const stamp = Date.now();
  const queryName = `Pulse ${stamp}`;
  const vizName = `Pulse ${stamp}`;
  const dashName = `e2e-pulse-dash-${stamp}`;
  let api: E2EApiClient;
  let dsId: number;
  let queryId: number;
  let vizId: number;
  let dashSlug: string;
  let dashId: number;
  let widgetId: number;

  const freshRun = () => executeAndWait(api, queryId, undefined, { maxAge: 0 });

  test.beforeAll(async () => {
    api = await loginAsAdmin();
    const ds = await api.createDataSource({
      name: `e2e-pulse-ds-${stamp}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    dsId = ds.id;
    const q = await api.createQuery({
      data_source_id: dsId,
      name: queryName,
      query: "SELECT clock_timestamp()::text AS ts, 1 AS n",
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
    const dash = await api.createDashboard(dashName);
    dashSlug = dash.slug;
    dashId = dash.id;
    const widget = await api.createWidget({
      dashboard_id: dashId,
      visualization_id: vizId,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
    });
    widgetId = (widget as { id: number }).id;
    await api.publishDashboard(dashSlug);
  });

  test.afterAll(async () => {
    await api.deleteDashboard(dashSlug).catch(() => {});
    await api.deleteVisualization(vizId).catch(() => {});
    await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(dsId).catch(() => {});
  });

  test("bell opens a Baseline monitor form that names the tile", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.goto(`/dashboard/${dashSlug}`);
    await page.getByRole("link", { name: `Add a monitor for ${vizName}` }).click();
    await expect(page).toHaveURL(
      new RegExp(
        `/monitors/new\\?query_id=${queryId}&from=tile&dashboard=${dashSlug}&widget=${widgetId}$`,
      ),
    );
    await expect(page.getByRole("button", { name: "Baseline", exact: true })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(page.getByText("Suggested for this tile")).toBeVisible();
    const note = page.getByRole("note");
    await expect(note).toContainText(vizName);
    await expect(note.getByRole("link", { name: dashName })).toHaveAttribute(
      "href",
      `/dashboard/${dashSlug}`,
    );

    await page.goto(`/monitors/new?query_id=${queryId}`);
    await expect(page.getByRole("button", { name: "Threshold", exact: true })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expect(page.getByText("Started from")).toHaveCount(0);
    await ctx.close();
  });

  test("new results pill refreshes the changed tile", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.clock.install();
    await page.goto(`/dashboard/${dashSlug}`);
    const tsCell = page.locator("td").filter({ hasText: /^\d{4}-\d{2}-\d{2}/ }).first();
    await expect(tsCell).toBeVisible({ timeout: 15_000 });
    const before = await tsCell.innerText();

    await freshRun();
    await page.clock.fastForward("01:05");

    const pill = page.getByRole("status").filter({ hasText: "New results available for 1 tile" });
    await expect(pill).toBeVisible({ timeout: 10_000 });
    await page.getByRole("button", { name: "Show new results" }).click();
    await expect(tsCell).not.toHaveText(before, { timeout: 10_000 });
    await expect(page.getByText("New results available")).toHaveCount(0);
    await ctx.close();
  });

  test("no pill in edit mode", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    await page.clock.install();
    await page.goto(`/dashboard/${dashSlug}`);
    await expect(page.locator("td").filter({ hasText: /^\d{4}-\d{2}-\d{2}/ }).first()).toBeVisible({
      timeout: 15_000,
    });
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await freshRun();
    await page.clock.fastForward("01:05");
    await page.clock.runFor(1000);
    await expect(page.getByText("New results available")).toHaveCount(0);
    await ctx.close();
  });
});
