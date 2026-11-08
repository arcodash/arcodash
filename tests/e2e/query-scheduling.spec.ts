/** E2E: query "Refresh schedule" section — schedule creation, worker-fired run, CSV download, Run
 * now, failed-run expander, auto-pause banner + resume. Requires: backend :8080, frontend :3000,
 * worker, target postgres :5434. */
import { readFileSync } from "node:fs";
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows, deleteRows } from "./helpers/db";
import { publishWake } from "./helpers/valkey";

async function expandScheduleSection(page: Page): Promise<void> {
  await page.getByRole("button", { name: /Refresh schedule/ }).click();
}

/** Poll runs for a matching status row, nudging the scheduler wake each iteration so a cron fire
 * isn't stuck behind the safety-cap sleep. */
async function waitForRunStatus(
  page: Page,
  status: "Success" | "Failed",
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await publishWake().catch(() => {});
    const visible = await page
      .getByText(status, { exact: true })
      .first()
      .isVisible()
      .catch(() => false);
    if (visible) return;
    await page.waitForTimeout(1000);
  }
  throw new Error(`no "${status}" run appeared within ${timeoutMs}ms`);
}

/** Poll the header FreshnessChip (not the section's "Run now" button) until its text matches
 * `pattern`, re-publishing the wake each iteration. */
async function waitForFreshnessChipText(page: Page, pattern: RegExp, timeoutMs: number): Promise<void> {
  const chip = page.getByTestId("freshness-chip");
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await publishWake().catch(() => {});
    const text = await chip.textContent({ timeout: 1000 }).catch(() => null);
    if (text && pattern.test(text)) return;
    await page.waitForTimeout(1000);
  }
  throw new Error(`freshness chip never matched ${pattern} within ${timeoutMs}ms`);
}

async function cleanup(api: E2EApiClient, queryId: number, dsId: number): Promise<void> {
  await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [queryId]).catch(() => {});
  await deleteRows("schedules", "entity_type = 'query' AND entity_id = $1", [queryId]).catch(
    () => {},
  );
  await api.deleteQuery(queryId).catch(() => {});
  await api.deleteDataSource(dsId).catch(() => {});
}

test.describe("query refresh schedule", () => {
  test("schedules a real query with a cron cadence, then Run now produces a real run, CSV download, and a second flip", async ({
    browser,
  }) => {
    test.setTimeout(90_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-sched-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-sched-q-${suffix}`,
      query: "SELECT int_col AS count FROM typed_table WHERE id = 1",
    });

    try {
      await api.publishQuery(q.id);

      await page.goto(`/queries/${q.id}`);
      await expandScheduleSection(page);

      const [createResp] = await Promise.all([
        page.waitForResponse(
          (r) => r.url().endsWith("/api/schedules") && r.request().method() === "POST",
        ),
        page.getByRole("switch").click(),
      ]);
      expect(createResp.ok()).toBeTruthy();
      await expect(page.getByText(/Next run:/)).toBeVisible();

      // Presets bottom out at 5 min; a 1-minute cadence needs the Advanced cron field. Run now
      // drives execution so timing doesn't depend on the scheduler loop.
      await page.getByText("▸ Advanced (cron)").click();
      const cronInput = page.getByPlaceholder("0 */15 * * * *");
      await cronInput.fill("0 * * * * *");
      const [patchResp] = await Promise.all([
        page.waitForResponse(
          (r) => /\/api\/schedules\/\d+$/.test(r.url()) && r.request().method() === "PATCH",
        ),
        cronInput.blur(),
      ]);
      expect(patchResp.ok()).toBeTruthy();
      await expect(page.getByText(/invalid cron/i)).not.toBeVisible();

      const [runResp] = await Promise.all([
        page.waitForResponse(
          (r) => r.url().endsWith(`/api/queries/${q.id}/run`) && r.request().method() === "POST",
        ),
        page.getByRole("button", { name: "Run now" }).click(),
      ]);
      expect(runResp.ok()).toBeTruthy();
      await expect(page.getByRole("button", { name: "Refreshing…" })).toBeVisible();
      await waitForRunStatus(page, "Success", 30_000);

      const csvLink = page.getByRole("link", { name: "⬇ CSV" }).first();
      const [download] = await Promise.all([page.waitForEvent("download"), csvLink.click()]);
      const filePath = await download.path();
      if (!filePath) throw new Error("download path is null");
      const content = readFileSync(filePath, "utf8");
      const lines = content.trim().split(/\r?\n/);
      expect(lines[0].toLowerCase()).toContain("count");
      expect(lines[1]).toContain("42");

      await page.getByRole("button", { name: "Run now" }).click();
      await expect(page.getByRole("button", { name: "Refreshing…" })).toBeVisible();
      await waitForRunStatus(page, "Success", 20_000);
    } finally {
      await cleanup(api, q.id, ds.id);
      await ctx.close();
    }
  });

  test("a genuinely scheduled fire (never Run now) flips the header freshness chip Refreshing… -> Updated … ago", async ({
    browser,
  }) => {
    // A real fire waits on the scheduler leader loop (6-25s+) plus the query's own runtime;
    // generous budget.
    test.setTimeout(120_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-sched-realfire-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    // A several-second query keeps "Refreshing…" observable instead of the poll racing past it.
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-sched-realfire-q-${suffix}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(8)) s",
    });

    try {
      await api.publishQuery(q.id);

      await page.goto(`/queries/${q.id}`);
      await expandScheduleSection(page);

      const [createResp] = await Promise.all([
        page.waitForResponse(
          (r) => r.url().endsWith("/api/schedules") && r.request().method() === "POST",
        ),
        page.getByRole("switch").click(),
      ]);
      expect(createResp.ok()).toBeTruthy();

      // Force the schedule due: proves a due schedule fires on its own (no "Run now"); cron timing
      // is covered by scheduler.spec.ts.
      await queryRows(
        "UPDATE schedules SET next_run_at = now() WHERE entity_type = 'query' AND entity_id = $1",
        [q.id],
      );

      // Reload so the first fetch sees the due next_run_at; polling only turns on when cached
      // next_run_at is within 15s, and the SQL update bypassed React.
      await page.reload();

      await waitForFreshnessChipText(page, /Refreshing/, 45_000);
      await waitForFreshnessChipText(page, /Updated/, 45_000);
    } finally {
      await cleanup(api, q.id, ds.id);
      await ctx.close();
    }
  });

  test("a failed run shows the verbatim connector error with a Retry button, and the header chip reads failed", async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-sched-fail-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-sched-fail-q-${suffix}`,
      query: "SELECT * FROM nonexistent_table_zzz",
    });

    try {
      await api.publishQuery(q.id);
      await page.goto(`/queries/${q.id}`);
      await expandScheduleSection(page);

      // The header chip renders only once a schedule row exists; toggle one on before the failing
      // run.
      const [createResp] = await Promise.all([
        page.waitForResponse(
          (r) => r.url().endsWith("/api/schedules") && r.request().method() === "POST",
        ),
        page.getByRole("switch").click(),
      ]);
      expect(createResp.ok()).toBeTruthy();

      await page.getByRole("button", { name: "Run now" }).click();
      await waitForRunStatus(page, "Failed", 30_000);

      await page.getByRole("button", { name: "Show error" }).first().click();
      const errorPre = page.locator("pre", { hasText: /nonexistent_table_zzz|does not exist/ });
      await expect(errorPre).toBeVisible();
      await expect(page.getByRole("button", { name: "Retry" })).toBeVisible();
      await expect(page.getByText(/Last refresh failed/)).toBeVisible();
    } finally {
      await cleanup(api, q.id, ds.id);
      await ctx.close();
    }
  });

  test("the auto-pause banner renders from a seeded paused schedule, and Resume clears it for real", async ({
    browser,
  }) => {
    test.setTimeout(60_000);
    // 10 real failures take ~10 minutes; seed the paused state to test the banner + Resume
    // (failure->auto-pause is covered by worker integration tests).
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-sched-pause-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-sched-pause-q-${suffix}`,
      query: "SELECT 1 AS n",
    });

    try {
      await api.publishQuery(q.id);

      const orgRows = await queryRows<{ org_id: number }>(
        "SELECT org_id FROM queries WHERE id = $1",
        [q.id],
      );
      const orgId = orgRows[0].org_id;

      await queryRows(
        `INSERT INTO schedules
           (org_id, entity_type, entity_id, spec, timezone, enabled, overlap_policy, next_run_at, auto_paused_at, consecutive_failures)
         VALUES ($1, 'query', $2, $3, 'UTC', false, 'skip', now(), now(), 10)`,
        [orgId, q.id, JSON.stringify({ kind: "interval", every_secs: 300 })],
      );
      await queryRows(
        `INSERT INTO runs
           (org_id, entity_type, entity_id, triggered_by, status, started_at, finished_at, duration_ms, error, created_at)
         VALUES ($1, 'query', $2, 'schedule', 'failed', now() - interval '1 hour', now() - interval '1 hour', 400, 'seeded connector failure', now())`,
        [orgId, q.id],
      );

      await page.goto(`/queries/${q.id}`);
      await expect(page.getByText("· Paused")).toBeVisible();

      await expandScheduleSection(page);
      await expect(page.getByText("Refreshes paused after 10 consecutive failures.")).toBeVisible();

      const [resumeResp] = await Promise.all([
        page.waitForResponse(
          (r) => /\/api\/schedules\/\d+$/.test(r.url()) && r.request().method() === "PATCH",
        ),
        page.getByRole("button", { name: "Resume" }).click(),
      ]);
      expect(resumeResp.ok()).toBeTruthy();
      await expect(
        page.getByText("Refreshes paused after 10 consecutive failures."),
      ).not.toBeVisible();

      const rows = await queryRows<{ enabled: boolean; auto_paused_at: string | null }>(
        "SELECT enabled, auto_paused_at FROM schedules WHERE entity_type = 'query' AND entity_id = $1",
        [q.id],
      );
      expect(rows[0].enabled).toBe(true);
      expect(rows[0].auto_paused_at).toBeNull();
    } finally {
      await cleanup(api, q.id, ds.id);
      await ctx.close();
    }
  });

  test("a typed but not-yet-committed cron edit survives a background schedule poll", async ({
    browser,
  }) => {
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-sched-typing-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-sched-typing-q-${suffix}`,
      query: "SELECT 1 AS n",
    });

    try {
      await api.publishQuery(q.id);
      await api.rawFetch("/api/schedules", {
        method: "POST",
        body: JSON.stringify({
          entity_type: "query",
          entity_id: q.id,
          spec: { kind: "cron", expr: "0 */5 * * * *" },
          enabled: true,
        }),
      });
      // Force the schedule due so the section's background poll turns on; only the polling matters
      // here.
      await queryRows(
        "UPDATE schedules SET next_run_at = now() WHERE entity_type = 'query' AND entity_id = $1",
        [q.id],
      );

      await page.goto(`/queries/${q.id}`);
      await expandScheduleSection(page);
      await page.getByText("▸ Advanced (cron)").click();
      const cronInput = page.getByPlaceholder("0 */15 * * * *");
      await expect(cronInput).toHaveValue("0 */5 * * * *", { timeout: 8000 });

      // Type without blurring: the poll must not reset the mid-edit field to the last-saved spec.
      await cronInput.fill("0 */7 * * * *");
      await page.waitForTimeout(7000); // at least two 3s poll ticks while the schedule is due
      await expect(cronInput).toHaveValue("0 */7 * * * *");
    } finally {
      await cleanup(api, q.id, ds.id);
      await ctx.close();
    }
  });
});
