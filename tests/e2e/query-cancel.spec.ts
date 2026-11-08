// pg_sleep(20) makes a banner well under 20s proof of a mid-flight worker abort, not just a
// job-status write.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, withTargetDs } from "./helpers/api";
import { expireJobStatusKey, jobStatusTtl } from "./helpers/valkey";

test("Stop aborts a mid-flight query in well under its own runtime, with no result table", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(60_000);
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `cancel-mid-flight-${Date.now()}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(20)) s",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.getByRole("button", { name: "Execute", exact: true }).click();

      const stopBtn = page.getByRole("button", { name: "Stop", exact: true });
      await expect(stopBtn).toBeVisible({ timeout: 8000 });

      const stoppedAt = Date.now();
      await stopBtn.click();

      await expect(page.getByText("Query stopped. No results were saved.")).toBeVisible({
        timeout: 8000,
      });
      const elapsedMs = Date.now() - stoppedAt;
      console.log(`[query-cancel] Stop click → cancelled banner: ${elapsedMs}ms`);
      // Proves the worker dropped the connector future instead of waiting out pg_sleep.
      expect(elapsedMs).toBeLessThan(8000);

      await expect(page.locator("table")).toHaveCount(0);

      await expect(page.getByRole("button", { name: "Execute", exact: true })).toBeEnabled();
      await expect(stopBtn).toHaveCount(0);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("Cancel still works after the job status record's TTL is aged near expiry", async ({
  browser,
}, testInfo) => {
  testInfo.setTimeout(60_000);
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `cancel-ttl-aged-${Date.now()}`,
      query: "SELECT count(*)::int AS n FROM (SELECT pg_sleep(10)) s",
    });
    try {
      await page.goto(`/queries/${q.id}`);

      const [response] = await Promise.all([
        page.waitForResponse(
          (res) => res.url().includes("/api/query_results") && res.request().method() === "POST",
        ),
        page.getByRole("button", { name: "Execute", exact: true }).click(),
      ]);
      const body = await response.json();
      const jobId: string = body.job?.id;
      expect(jobId, `execute response: ${JSON.stringify(body)}`).toBeTruthy();

      // Age the status record's TTL below the poll loop's 1s refresh to simulate near-expiry
      // mid-run.
      await expect
        .poll(() => jobStatusTtl(jobId), { timeout: 8000, intervals: [200] })
        .toBeGreaterThan(0);
      await expireJobStatusKey(jobId, 2);

      // Without renewal the key is gone within 2s; seeing it pushed back up proves the poll loop
      // renews it.
      await expect
        .poll(() => jobStatusTtl(jobId), { timeout: 6000, intervals: [500] })
        .toBeGreaterThan(60);

      const stopBtn = page.getByRole("button", { name: "Stop", exact: true });
      await expect(stopBtn).toBeVisible();

      const stoppedAt = Date.now();
      await stopBtn.click();

      // If the aged-out record made set_cancelled_if_not_terminal a no-op, Stop would hang until
      // pg_sleep ends; a fast banner proves Cancel still works.
      await expect(page.getByText("Query stopped. No results were saved.")).toBeVisible({
        timeout: 8000,
      });
      expect(Date.now() - stoppedAt).toBeLessThan(8000);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});
