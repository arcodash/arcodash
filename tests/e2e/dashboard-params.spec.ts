/** E2E: DashboardParamsBar typed controls (date-range + dropdown). Requires: backend :8080,
 * frontend :3000, target postgres :5434 (typed_table seed in tests/e2e/seed/target-db.sql: row 1
 * date 2025-01-01/int 42/'hello', row 2 date 1970-01-01/int 0/''). */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";

/** Persist a real parameter definition on query.options.parameters; a bare {{token}} isn't enough
 * for executeQuery({parameters}) to accept a value. */
async function setQueryParameters(
  api: E2EApiClient,
  queryId: number,
  parameters: unknown[],
): Promise<void> {
  const full = await api.rawFetch<{ working_version: number }>(`/api/queries/${queryId}`);
  await api.rawFetch(`/api/queries/${queryId}`, {
    method: "POST",
    body: JSON.stringify({ options: { parameters }, expected_version: full.working_version }),
  });
}

test("dashboard date-range and dropdown params: typed controls, real refetch, URL persistence, reload restore", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();

  const ds = await api.createDataSource({
    name: `e2e-dash-params-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  // DISTINCT: repeated non-idempotent seed runs can duplicate rows; collapse to one cell per value.
  const dateQuery = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-dash-params-daterange-${stamp}`,
    query:
      "SELECT DISTINCT int_col FROM typed_table WHERE date_col BETWEEN '{{ d.start }}' AND '{{ d.end }}' ORDER BY int_col",
  });
  const dropdownQuery = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-dash-params-dropdown-${stamp}`,
    query: "SELECT DISTINCT text_col FROM typed_table WHERE text_col = '{{ region }}'",
  });

  await setQueryParameters(api, dateQuery.id, [
    { name: "d", title: "Date range", type: "date-range", value: { start: "1970-01-01", end: "1970-01-01" } },
  ]);
  await setQueryParameters(api, dropdownQuery.id, [
    { name: "region", title: "Region", type: "dropdown", enum_options: "hello", value: "hello" },
  ]);
  await api.publishQuery(dateQuery.id);
  await api.publishQuery(dropdownQuery.id);

  // Seed a cached result matching each default so the widget shows no spurious fetch error.
  await executeAndWait(api, dateQuery.id, { d: { start: "1970-01-01", end: "1970-01-01" } });
  await executeAndWait(api, dropdownQuery.id, { region: "hello" });

  const dateViz = await api.createVisualization({
    query_id: dateQuery.id,
    type: "table",
    name: `e2e-dash-params-daterange-viz-${stamp}`,
    options: {},
  });
  const dropdownViz = await api.createVisualization({
    query_id: dropdownQuery.id,
    type: "table",
    name: `e2e-dash-params-dropdown-viz-${stamp}`,
    options: {},
  });

  const dash = await api.createDashboard(`e2e-dash-params-${stamp}`);

  try {
    await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: dateViz.id,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: dropdownViz.id,
      text: null,
      options: { position: { col: 3, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await api.publishDashboard(dash.slug);

    await page.goto(`/dashboard/${dash.slug}`);

    await expect(page.locator("td").filter({ hasText: /^0$/ })).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: /^42$/ })).not.toBeVisible();
    await expect(page.locator("td").filter({ hasText: /^hello$/ })).toBeVisible({ timeout: 15_000 });

    const dateControl = page.getByTestId("dashboard-param-d");
    const dateInputs = dateControl.locator('input[type="date"]');
    await expect(dateInputs).toHaveCount(2);
    await expect(dateInputs.nth(0)).toHaveValue("1970-01-01");
    await expect(dateInputs.nth(1)).toHaveValue("1970-01-01");

    const dropdownControl = page.getByTestId("dashboard-param-region");
    await expect(dropdownControl.locator("input")).toHaveCount(0);
    await expect(dropdownControl.getByText("hello")).toBeVisible();

    await dateInputs.nth(1).fill("2025-12-31");
    await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: /^0$/ })).toBeVisible();
    await expect(page.getByText(/Failed to load data/i)).not.toBeVisible();

    await expect(page).toHaveURL(/p_d=/);
    await expect(page).toHaveURL(/p_region=hello/);
    const url = new URL(page.url());
    const dParam = JSON.parse(url.searchParams.get("p_d")!);
    expect(dParam).toEqual({ start: "1970-01-01", end: "2025-12-31" });

    await page.reload();
    await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: /^0$/ })).toBeVisible();
    await expect(page.getByTestId("dashboard-param-d").locator('input[type="date"]').nth(1)).toHaveValue(
      "2025-12-31",
    );
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
    await api.deleteVisualization(dateViz.id).catch(() => {});
    await api.deleteVisualization(dropdownViz.id).catch(() => {});
    await api.deleteQuery(dateQuery.id).catch(() => {});
    await api.deleteQuery(dropdownQuery.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
