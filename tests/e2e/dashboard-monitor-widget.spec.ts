/** E2E: dashboards + monitor widgets (options.monitors). One long test: the fixture is expensive
 * and state carries across phases (signed-in render, ok->triggered flip, anonymous embed).
 * Requires: backend :8080, frontend :3000, target postgres :5434, test metadata postgres :5433,
 * Valkey :6380. */
import { test, expect, type Page, type Locator } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { publishWake } from "./helpers/valkey";
import { enterEditMode } from "./helpers/actions";

/** Force the monitor's schedule due and nudge the deadline engine (the default schedule fires every
 * 900s). */
async function forceMonitorScheduleNow(monitorId: number): Promise<void> {
  await queryRows(
    "UPDATE schedules SET next_run_at = now() WHERE entity_type = 'monitor' AND entity_id = $1",
    [monitorId],
  );
  await publishWake().catch(() => {});
}

/** Force a schedule-driven evaluation and poll monitors.state until it equals expected. Not the raw
 * Valkey push: that updates state but writes no runs row. */
async function evaluateAndWait(monitorId: number, expected: string): Promise<void> {
  await forceMonitorScheduleNow(monitorId);
  await expect
    .poll(
      async () => {
        const r = await queryRows<{ state: string }>(
          "SELECT state FROM monitors WHERE id = $1",
          [monitorId],
        );
        return r[0]?.state;
      },
      { timeout: 90_000, intervals: [2000] },
    )
    .toBe(expected);
}

/** Force a re-evaluation and poll until the newest terminal run's observed cell for `column` equals
 * expected; needed when state doesn't flip, so the state poll can't prove a fresh run. */
async function waitForObserved(
  api: E2EApiClient,
  monitorId: number,
  column: string,
  expected: string,
): Promise<void> {
  await forceMonitorScheduleNow(monitorId);
  await expect
    .poll(
      async () => {
        const r = await queryRows<{ qr: number | null }>(
          `SELECT r.query_result_id AS qr
           FROM runs r
           WHERE r.entity_type = 'monitor' AND r.entity_id = $1
             AND r.status IN ('success','failed','timed_out','cancelled','skipped')
           ORDER BY r.id DESC LIMIT 1`,
          [monitorId],
        );
        const qrId = r[0]?.qr;
        if (qrId == null) return null;
        const { query_result } = await api.rawFetch<{
          query_result: { data: { rows: Record<string, unknown>[] } };
        }>(`/api/query_results/${qrId}`);
        const v = query_result.data.rows[0]?.[column];
        return v == null ? null : String(v);
      },
      { timeout: 90_000, intervals: [2000] },
    )
    .toBe(expected);
}

/** Scope to the ActionBar: the empty-state placeholder has its own "Add Widget" button. */
async function openAddWidgetDialog(page: Page) {
  await page.getByTestId("actionbar-root").getByRole("button", { name: "Add Widget", exact: true }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  return dialog;
}

/** Drive AddWidgetDialog's Monitor mode: filter by filterText and check each match. */
async function addMonitorWidget(
  page: Page,
  filterText: string,
  monitorNames: string[],
): Promise<void> {
  const dialog = await openAddWidgetDialog(page);
  await dialog.getByRole("button", { name: "Monitor", exact: true }).click();
  await dialog.getByTestId("add-widget-monitor-filter").fill(filterText);
  for (const name of monitorNames) {
    await expect(dialog.getByText(name, { exact: true })).toBeVisible({ timeout: 8000 });
    await dialog.getByText(name, { exact: true }).click();
  }
  await dialog.getByRole("button", { name: "Add", exact: true }).click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
}

function singleWidgetBody(page: Page) {
  return page
    .getByTestId("monitor-widget")
    .filter({ hasNot: page.locator('[data-testid="monitor-widget-row"]') });
}

function listWidgetBody(page: Page) {
  return page
    .getByTestId("monitor-widget")
    .filter({ has: page.locator('[data-testid="monitor-widget-row"]') });
}

/** Walk up from the body to the WidgetCard root, which carries the destructive/warning/neutral
 * chrome. */
function cardRoot(body: Locator) {
  return body.locator("xpath=ancestor::div[contains(@class,'rounded-md')][1]");
}

test("monitor widget: create via UI, flip a monitor state for real, then read it back anonymously through a public embed", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(240_000);

  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const consoleErrors: string[] = [];

  const stamp = Date.now();
  const ds = await api.createDataSource({
    name: `e2e-mw-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-mw-q-${stamp}`,
    query: "SELECT 1 AS count",
  });
  await api.publishQuery(q.id);
  // Unedited since publish, this repoints the PUBLISHED pointer, so no second publish is needed.
  await executeAndWait(api, q.id);

  const nameA = `e2e-mw-a-${stamp}`;
  const nameB = `e2e-mw-b-${stamp}`;
  // A: 1 > 5 starts "ok"; phase 2 flips it by editing the query.
  const monitorA = await api.createMonitor({
    query_id: q.id,
    name: nameA,
    options: { column: "count", op: ">", value: 5 },
  });
  // B: 1 > 0 starts and stays "triggered", giving phase 1 an Alerting/OK pair in one list widget.
  const monitorB = await api.createMonitor({
    query_id: q.id,
    name: nameB,
    options: { column: "count", op: ">", value: 0 },
  });

  let monitorLong: { id: number } | undefined;
  try {
    await evaluateAndWait(monitorA.id, "ok");
    await evaluateAndWait(monitorB.id, "triggered");

    const dash = await api.createDashboard(`e2e-mw-dash-${stamp}`);
    try {
      await page.goto(`/dashboard/${dash.slug}`);
      await enterEditMode(page);

      await addMonitorWidget(page, nameA, [nameA]);
      await addMonitorWidget(page, String(stamp), [nameA, nameB]);

      const single = singleWidgetBody(page);
      const list = listWidgetBody(page);
      await expect(single).toBeVisible({ timeout: 10_000 });
      await expect(list).toBeVisible({ timeout: 10_000 });

      await expect(single.getByTestId("monitor-widget-state")).toHaveText("OK");
      await expect(single.getByTestId("monitor-widget-value")).toHaveText("1");

      const rowA = list.getByTestId("monitor-widget-row").filter({ hasText: nameA });
      const rowB = list.getByTestId("monitor-widget-row").filter({ hasText: nameB });
      await expect(rowA.getByTestId("monitor-widget-state")).toHaveText("OK");
      await expect(rowB.getByTestId("monitor-widget-state")).toHaveText("Alerting");
      await expect(rowA).toContainText("1");
      await expect(rowB).toContainText("1");

      // execute always runs the PUBLISHED content, so publish must land BEFORE execute or the draft
      // edit is ignored.
      await api.updateQuery(q.id, { query: "SELECT 10 AS count" });
      await api.publishQuery(q.id);
      await executeAndWait(api, q.id);
      await evaluateAndWait(monitorA.id, "triggered");
      // B's state doesn't flip and its 900s schedule never fires; force + wait for its run.
      await waitForObserved(api, monitorB.id, "count", "10");

      // Long monitor names truncate in a 1-col card; hover reveals the full title.
      const longName = `e2e-mw-long-${stamp}-${"very-long-monitor-name-".repeat(3)}`;
      monitorLong = await api.createMonitor({
        query_id: q.id,
        name: longName,
        options: { column: "count", op: ">", value: 5 },
      });
      const longWidget = await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: null,
        text: null,
        options: {
          monitors: [monitorLong.id],
          position: { col: 0, row: 30, sizeX: 1, sizeY: 2 },
        },
      });

      await page.reload();
      const longTitle = page.getByText(longName, { exact: true }).first();
      await longTitle.scrollIntoViewIfNeeded();
      await longTitle.hover();
      await expect(page.getByRole("tooltip")).toHaveText(longName);

      // Removed so the single/list card locators below stay unambiguous.
      await api.deleteWidget(longWidget.id);
      await page.reload();
      await expect(singleWidgetBody(page).getByTestId("monitor-widget-state")).toHaveText(
        "Alerting",
        { timeout: 10_000 },
      );
      await expect(singleWidgetBody(page).getByTestId("monitor-widget-value")).toHaveText("10");
      await expect(cardRoot(singleWidgetBody(page))).toHaveClass(/border-destructive/);

      const rowAAfter = listWidgetBody(page)
        .getByTestId("monitor-widget-row")
        .filter({ hasText: nameA });
      await expect(rowAAfter.getByTestId("monitor-widget-state")).toHaveText("Alerting");
      await expect(rowAAfter).toContainText("10");

      const dashUrl = page.url();
      // The stretched link overlays the whole card, so the click must land on what covers the
      // point.
      await singleWidgetBody(page).click({ force: true });
      await expect(page).toHaveURL(new RegExp(`/monitors/${monitorA.id}$`));
      await page.goBack();
      await expect(page).toHaveURL(dashUrl);

      await rowAAfter.click({ force: true });
      await expect(page).toHaveURL(new RegExp(`/monitors/${monitorA.id}$`));
      await page.goBack();
      await expect(page).toHaveURL(dashUrl);

      await enterEditMode(page);
      await expect(cardRoot(singleWidgetBody(page)).getByRole("link")).toHaveCount(0);
      await expect(cardRoot(listWidgetBody(page)).getByRole("link")).toHaveCount(0);
      await page.reload();

      await api.publishDashboard(dash.slug);
      const { public_url } = await api.shareDashboard(dash.slug);

      const anon = await browser.newContext();
      try {
        const anonPage = await anon.newPage();
        anonPage.on("console", (msg) => {
          if (msg.type() !== "error") return;
          const text = msg.text();
          // Dev-server artifact unrelated to the page under test.
          if (text.includes("process is not defined")) return;
          consoleErrors.push(text);
        });

        await anonPage.goto(public_url);
        await expect(anonPage.getByText(dash.name)).toBeVisible({ timeout: 10_000 });

        const anonSingle = singleWidgetBody(anonPage);
        const anonList = listWidgetBody(anonPage);
        await expect(anonSingle).toBeVisible({ timeout: 10_000 });
        await expect(anonList).toBeVisible({ timeout: 10_000 });

        await expect(anonSingle.getByTestId("monitor-widget-state")).toHaveText("Alerting");
        await expect(anonSingle.getByTestId("monitor-widget-value")).toHaveText("10");

        const anonRowA = anonList.getByTestId("monitor-widget-row").filter({ hasText: nameA });
        const anonRowB = anonList.getByTestId("monitor-widget-row").filter({ hasText: nameB });
        await expect(anonRowA.getByTestId("monitor-widget-state")).toHaveText("Alerting");
        await expect(anonRowB.getByTestId("monitor-widget-state")).toHaveText("Alerting");
        await expect(anonRowA).toContainText("10");
        await expect(anonRowB).toContainText("10");
        await expect(cardRoot(anonSingle).getByRole("link")).toHaveCount(0);
        await expect(cardRoot(anonList).getByRole("link")).toHaveCount(0);

        expect(consoleErrors, `unexpected console errors: ${consoleErrors.join("; ")}`).toEqual(
          [],
        );
      } finally {
        await anon.close();
      }
    } finally {
      await api.revokeShareDashboard(dash.slug).catch(() => {});
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  } finally {
    await api.deleteMonitor(monitorA.id).catch(() => {});
    await api.deleteMonitor(monitorB.id).catch(() => {});
    if (monitorLong) await api.deleteMonitor(monitorLong.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
