// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, withTargetDs } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { typeSql } from "./helpers/editor";
import { clickActionBarItem, selectDataSource } from "./helpers/actions";

/** Reads the execution-state + result-pointer columns that stay null for an ad-hoc run. */
async function getExecutionState(queryId: number) {
  const rows = await queryRows<{
    last_execution_status: string;
    last_execution_at: string | null;
    latest_query_data_id: number | null;
  }>(
    "SELECT last_execution_status, last_execution_at, latest_query_data_id FROM queries WHERE id = $1",
    [queryId],
  );
  return rows[0];
}

test("first Execute on a new query (real name + SQL) binds, publishes clean, and feeds a working dashboard widget", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const queryName = `e2e-first-run-${stamp}`;

  await withTargetDs(api, async (_dsId, dsName) => {
    let queryId: number | null = null;
    let vizId: number | null = null;
    let dashSlug: string | null = null;

    try {
      await page.goto("/queries/new");
      await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });
      await page.fill("#query-name", queryName);
      await selectDataSource(page, dsName);
      // Real SQL satisfies the autosave gate, but Execute must still force a save first or the run
      // goes out ad-hoc with no query_id.
      await typeSql(page, "SELECT 42 AS val");

      await page.click('button:has-text("Execute")');
      await page.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
      queryId = Number(page.url().match(/\/queries\/(\d+)/)![1]);

      await expect
        .poll(async () => (await getExecutionState(queryId!))?.last_execution_status, {
          timeout: 10_000,
        })
        .toBe("Success");
      const state = await getExecutionState(queryId);
      expect(state.latest_query_data_id).not.toBeNull();
      expect(state.last_execution_at).not.toBeNull();

      // The dialog reads the page's cached query, which lags the DB; wait for Success before
      // opening Publish.
      await expect(async () => {
        await page.getByRole("button", { name: "Query details" }).hover();
        await expect(page.getByRole("tooltip").getByText(/^Success,/)).toBeVisible({ timeout: 1000 });
      }).toPass({ timeout: 15_000 });
      await page.mouse.move(0, 0);

      await clickActionBarItem(page, "Publish");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await expect(
        dialog.getByText(/haven't run this query successfully yet/i),
      ).not.toBeVisible();
      await dialog.getByRole("button", { name: "Publish", exact: true }).click();
      await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

      const viz = await api.createVisualization({
        query_id: queryId,
        type: "table",
        name: `e2e-first-run-viz-${stamp}`,
        options: {},
      });
      vizId = viz.id;

      const dash = await api.createDashboard(`e2e-first-run-dash-${stamp}`);
      dashSlug = dash.slug;

      await page.goto(`/dashboard/${dash.slug}`);
      await page.click('button:has-text("Edit")');
      await page.click('button:has-text("Add Widget")');
      await expect(page.locator("[role=dialog]")).toBeVisible({ timeout: 8000 });

      const querySelect = page.getByTestId("add-widget-query-select");
      await querySelect.click();
      await page.getByRole("option", { name: queryName }).click();
      await expect(querySelect).toHaveAttribute("aria-expanded", "false");

      const vizSelect = page.getByTestId("add-widget-viz-select");
      await expect(vizSelect).toBeVisible({ timeout: 8000 });
      await vizSelect.click();
      await page.getByRole("option").first().click();
      await expect(vizSelect).toHaveAttribute("aria-expanded", "false");

      const addButton = page.locator('[role=dialog] button:has-text("Add")');
      await expect(addButton).toBeEnabled();
      await addButton.click();
      await expect(page.locator("[role=dialog]")).not.toBeVisible({ timeout: 8000 });

      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(page.getByText("Failed to load data")).not.toBeVisible();

      await page.reload();
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(page.getByText("Failed to load data")).not.toBeVisible();

      await page.goto("/queries");
      const tableRow = page.locator("tr", { has: page.getByText(queryName, { exact: true }) });
      await expect(tableRow.getByText("Success", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(tableRow.getByText("Never")).not.toBeVisible();
    } finally {
      if (dashSlug) await api.deleteDashboard(dashSlug).catch(() => {});
      if (vizId) await api.deleteVisualization(vizId).catch(() => {});
      if (queryId) await api.deleteQuery(queryId).catch(() => {});
    }
  });

  await ctx.close();
});

test("a valid 0-row query still publishes clean and shows a legitimate Success/0-row state, not Failed", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const queryName = `e2e-first-run-zero-rows-${stamp}`;

  await withTargetDs(api, async (_dsId, dsName) => {
    let queryId: number | null = null;
    try {
      await page.goto("/queries/new");
      await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });
      await page.fill("#query-name", queryName);
      await selectDataSource(page, dsName);
      await typeSql(page, "SELECT 1 AS val WHERE false");

      await page.click('button:has-text("Execute")');
      await page.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
      queryId = Number(page.url().match(/\/queries\/(\d+)/)![1]);

      // A 0-row result is still a successful run.
      await expect
        .poll(async () => (await getExecutionState(queryId!))?.last_execution_status, {
          timeout: 10_000,
        })
        .toBe("Success");
      const state = await getExecutionState(queryId);
      expect(state.latest_query_data_id).not.toBeNull();

      await clickActionBarItem(page, "Publish");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await expect(
        dialog.getByText(/haven't run this query successfully yet/i),
      ).not.toBeVisible();
      await dialog.getByRole("button", { name: "Publish", exact: true }).click();
      await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

      await page.goto("/queries");
      const tableRow = page.locator("tr", { has: page.getByText(queryName, { exact: true }) });
      await expect(tableRow.getByText("Success", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(tableRow.getByText("Failed", { exact: true })).not.toBeVisible();
      await expect(tableRow.getByText("Never")).not.toBeVisible();
    } finally {
      if (queryId) await api.deleteQuery(queryId).catch(() => {});
    }
  });

  await ctx.close();
});
