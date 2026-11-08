// Requires: backend :8080, frontend :3000, worker (redis :6380), target postgres :5434, mailpit http :8026 / smtp :1026.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, executeAndWait, TARGET_DS_OPTIONS } from "./helpers/api";
import { deleteRows } from "./helpers/db";
import { publishWake } from "./helpers/valkey";
import { pollMailpitForSubject } from "./helpers/mailpit";

// options takes raw JSON (staleness/fresh-wait secs aren't in the typed createMonitor options).
async function createMonitorRaw(
  api: E2EApiClient,
  body: {
    query_id: number;
    name: string;
    options: Record<string, unknown>;
  },
): Promise<{ id: number; name: string }> {
  return api.rawFetch<{ id: number; name: string }>("/api/monitors", {
    method: "POST",
    body: JSON.stringify(body),
  });
}

async function updateQuerySql(api: E2EApiClient, queryId: number, sql: string): Promise<void> {
  await api.rawFetch(`/api/queries/${queryId}`, {
    method: "POST",
    body: JSON.stringify({ query: sql }),
  });
}

// The freshness check truncates to whole seconds, so a 1s ceiling checked in the seed's sub-second
// window reads age=0 and takes the fast path; wait past the ceiling.
async function waitPastStaleness(ms = 2500): Promise<void> {
  await new Promise((r) => setTimeout(r, ms));
}

async function cleanup(api: E2EApiClient, ids: { monitorId?: number; queryId: number; dsId: number }): Promise<void> {
  if (ids.monitorId != null) {
    await deleteRows("runs", "entity_type = 'monitor' AND entity_id = $1", [ids.monitorId]).catch(() => {});
    await deleteRows("schedules", "entity_type = 'monitor' AND entity_id = $1", [ids.monitorId]).catch(() => {});
    await api.deleteMonitor(ids.monitorId).catch(() => {});
  }
  await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [ids.queryId]).catch(() => {});
  await deleteRows("schedules", "entity_type = 'query' AND entity_id = $1", [ids.queryId]).catch(() => {});
  await api.deleteQuery(ids.queryId).catch(() => {});
  await api.deleteDataSource(ids.dsId).catch(() => {});
}

test.describe("monitor check schedule", () => {
  test("a monitor on a slow-refresh query shows the waiting badge, then resolves", async ({ browser }) => {
    test.setTimeout(90_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-wait-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-wait-q-${suffix}`,
      query: "SELECT pg_sleep(8), 1 AS count",
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);
      // Seed an aging result so the first check finds an existing, too-stale result and triggers a
      // refresh (no result evaluates as "unknown").
      await executeAndWait(api, q.id);
      await waitPastStaleness();

      const monitor = await createMonitorRaw(api, {
        query_id: q.id,
        name: `e2e-mon-sched-wait-${suffix}`,
        options: { column: "count", op: ">", value: 0, max_data_staleness_secs: 1 },
      });
      monitorId = monitor.id;

      await page.goto(`/monitors/${monitor.id}`);
      await expect(page.getByText("Check schedule")).toBeVisible();

      await page.getByRole("button", { name: "Check now" }).click();

      // Nudge the deadline-scheduler's wake channel so the parked state isn't stuck behind a slow
      // poll.
      const waitingBadge = page.getByText("Waiting for fresh data…");
      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            return waitingBadge.isVisible().catch(() => false);
          },
          { timeout: 20_000, intervals: [1000] },
        )
        .toBe(true);

      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            return waitingBadge.isVisible().catch(() => false);
          },
          { timeout: 30_000, intervals: [1500] },
        )
        .toBe(false);
      // exact: true, or "Alerting" also matches the Notify sub-label copy.
      await expect(
        page.getByText("Alerting", { exact: true }).or(page.getByText("OK", { exact: true })).first(),
      ).toBeVisible({ timeout: 10_000 });
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await ctx.close();
    }
  });

  test("a triggered monitor shows Alerting + Condition met + Sent to, with a real Mailpit email", async ({
    browser,
  }) => {
    test.setTimeout(150_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();
    const monitorName = `e2e-mon-sched-trigger-${suffix}`;

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-trigger-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-trigger-q-${suffix}`,
      query: "SELECT 100 AS count",
    });
    const destination = await api.createDestination({
      name: `e2e-mon-sched-trigger-dest-${suffix}`,
      type: "email",
      options: { to: `${monitorName}@example.com` },
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);
      await executeAndWait(api, q.id);

      const monitor = await createMonitorRaw(api, {
        query_id: q.id,
        name: monitorName,
        options: { column: "count", op: ">", value: 50 },
      });
      monitorId = monitor.id;
      await api.createMonitorSubscription(monitor.id, { destination_id: destination.id });

      await page.goto(`/monitors/${monitor.id}`);
      await page.getByRole("button", { name: "Check now" }).click();

      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            // exact: true, or "Alerting" also matches the Notify sub-label copy.
            return page.getByText("Alerting", { exact: true }).isVisible().catch(() => false);
          },
          { timeout: 30_000, intervals: [1500] },
        )
        .toBe(true);

      await expect(page.getByText("Condition met")).toBeVisible();

      await expect(page.getByText("Not checked yet")).toBeHidden({ timeout: 10_000 });
      await expect(page.getByText(/^Sent to /)).toBeVisible({ timeout: 60_000 });

      // The delivery is recorded by a separate downstream SendNotification job that can lag the
      // run's finalize; poll the API until confirmed, then reload once and assert.
      await expect
        .poll(
          async () => {
            const runs = await page.evaluate(
              (mid) => fetch(`/api/runs?entity_type=monitor&entity_id=${mid}`).then((r) => r.json()),
              monitor.id,
            );
            return (runs as { notified: { status: string }[] }[]).some((r) =>
              r.notified.some((n) => n.status === "sent"),
            );
          },
          { timeout: 60_000, intervals: [2000] },
        )
        .toBe(true);
      await page.reload();
      await expect(page.getByText(/^Sent to /)).toBeVisible({ timeout: 10_000 });

      const message = await pollMailpitForSubject(monitorName, 90_000);
      expect(message.Subject).toContain(monitorName);
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await api.deleteDestination(destination.id).catch(() => {});
      await ctx.close();
    }
  });

  test("a broken destination shows 'Failed: <destination>' in the runs table, not only Delivery history", async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-fail-dest-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-fail-dest-q-${suffix}`,
      query: "SELECT 100 AS count",
    });
    // No `url` in options: an immediate, permanent config error (never retried).
    const destination = await api.createDestination({
      name: `e2e-mon-sched-fail-dest-${suffix}`,
      type: "webhook",
      options: {},
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);
      await executeAndWait(api, q.id);

      const monitor = await createMonitorRaw(api, {
        query_id: q.id,
        name: `e2e-mon-sched-fail-dest-${suffix}`,
        options: { column: "count", op: ">", value: 50 },
      });
      monitorId = monitor.id;
      await api.createMonitorSubscription(monitor.id, { destination_id: destination.id });

      await page.goto(`/monitors/${monitor.id}`);
      await page.getByRole("button", { name: "Check now" }).click();

      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            return page.getByText(`Failed: ${destination.name}`).isVisible().catch(() => false);
          },
          { timeout: 30_000, intervals: [1500] },
        )
        .toBe(true);

      const failedCell = page.getByText(`Failed: ${destination.name}`);
      await expect(failedCell).toHaveClass(/text-destructive/);
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await api.deleteDestination(destination.id).catch(() => {});
      await ctx.close();
    }
  });

  test("a max-wait ceiling detaches into a stale-fallback amber Data-age row", async ({ browser }) => {
    // The ceiling-detach sweep runs at most every 60s (deadline.rs::SWEEP_INTERVAL_SECS), so this
    // test budgets generously.
    test.setTimeout(150_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-stale-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-stale-q-${suffix}`,
      query: "SELECT 5 AS count",
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);
      // Seed a fast completed result first; the stale fallback reads it back.
      await executeAndWait(api, q.id);
      await waitPastStaleness();

      const monitor = await createMonitorRaw(api, {
        query_id: q.id,
        name: `e2e-mon-sched-stale-${suffix}`,
        options: {
          column: "count",
          op: ">",
          value: 0,
          max_data_staleness_secs: 1,
          max_fresh_wait_secs: 1,
        },
      });
      monitorId = monitor.id;

      // Swap in the slow refresh after seeding so the next check parks behind it past the 1s
      // ceiling.
      await updateQuerySql(api, q.id, "SELECT pg_sleep(75), 5 AS count");
      await api.publishQuery(q.id);

      await page.goto(`/monitors/${monitor.id}`);
      await page.getByRole("button", { name: "Check now" }).click();

      const staleCell = page.getByText(/^data .+ old$/);
      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            return staleCell.first().isVisible().catch(() => false);
          },
          { timeout: 130_000, intervals: [3000] },
        )
        .toBe(true);

      await expect(staleCell.first()).toHaveClass(/text-warning-foreground/);
      const title = await staleCell.first().getAttribute("title");
      expect(title).toContain("ran against data from");
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await ctx.close();
    }
  });

  test("a failed-to-check row never shares the alerting badge", async ({ browser }) => {
    test.setTimeout(60_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-fail-check-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-fail-check-q-${suffix}`,
      query: "SELECT 1 AS count",
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);
      await executeAndWait(api, q.id);
      await waitPastStaleness();

      const monitor = await createMonitorRaw(api, {
        query_id: q.id,
        name: `e2e-mon-sched-fail-check-${suffix}`,
        options: { column: "count", op: ">", value: 0, max_data_staleness_secs: 1 },
      });
      monitorId = monitor.id;

      // Break the SQL after the seed, which needed valid SQL.
      await updateQuerySql(api, q.id, "SELECT * FROM nonexistent_table_zzz_scheduling");
      await api.publishQuery(q.id);

      await page.goto(`/monitors/${monitor.id}`);
      await page.getByRole("button", { name: "Check now" }).click();

      const failedRow = page.getByText("Failed to run");
      await expect
        .poll(
          async () => {
            await publishWake().catch(() => {});
            return failedRow.isVisible().catch(() => false);
          },
          { timeout: 30_000, intervals: [1500] },
        )
        .toBe(true);

      await expect(failedRow).toHaveClass(/text-destructive/);
      // exact: true, or "Alerting" also matches the Notify sub-label copy.
      expect(await page.getByText("Alerting", { exact: true }).count()).toBe(0);

      await page.getByRole("button", { name: "Show error" }).first().click();
      const errorPre = page.locator("pre", { hasText: /query run #\d+ failed: / });
      await expect(errorPre).toBeVisible();
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await ctx.close();
    }
  });

  test("a new monitor defaults to check-on, every 15 min, with no checks yet", async ({ browser }) => {
    test.setTimeout(60_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-mon-sched-new-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-mon-sched-new-q-${suffix}`,
      query: "SELECT 1 AS count",
    });

    let monitorId: number | undefined;
    try {
      await api.publishQuery(q.id);

      await executeAndWait(api, q.id);
      await page.goto("/monitors/new");
      await page.getByPlaceholder("Monitor name").fill(`e2e-mon-sched-new-${suffix}`);
      // Not combobox.first(): the comparison select precedes the query select until the queries
      // list loads.
      await page.getByTestId("monitor-query-select").click();
      await page.getByRole("option", { name: q.name }).click();
      await page.getByTestId("monitor-column-select").click();
      await page.getByRole("option", { name: "count" }).click();
      await page.getByRole("spinbutton").fill("0");

      await page.getByRole("button", { name: /^save$/i }).click();
      await page.waitForURL(/\/monitors\/\d+$/, { timeout: 15_000 });
      monitorId = Number(page.url().match(/\/monitors\/(\d+)$/)![1]);

      await expect(page.getByText("Check schedule")).toBeVisible();
      await expect(page.getByRole("switch").first()).toBeChecked();
      // The collapsed summary and expanded select both contain "Every 15 min"; match the first.
      await expect(page.getByText("Every 15 min").first()).toBeVisible();
      await expect(page.getByText("No checks yet — the first check will appear here.")).toBeVisible();
    } finally {
      await cleanup(api, { monitorId, queryId: q.id, dsId: ds.id });
      await ctx.close();
    }
  });
});
