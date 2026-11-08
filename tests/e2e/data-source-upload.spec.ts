// Requires: backend :8080, frontend :3000, fake-gcs-server :4444 (docker-compose.testing.yml
// `gcs`).
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { queryRows } from "./helpers/db";

test("upload a CSV, save the data source, and run a query against it", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const dsName = `e2e-csv-upload-${Date.now()}`;

  try {
    await page.goto("/data_sources/new");
    await page.fill("#ds-name", dsName);
    await page.click("#ds-type");
    await page.getByRole("option", { name: "CSV" }).click();

    await page.setInputFiles(
      '[data-testid="file-upload-input"]',
      "tests/e2e/fixtures/sample.csv",
    );

    await expect(page.getByText("sample.csv")).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: "alice" })).toBeVisible();
    await expect(page.locator("td").filter({ hasText: "bob" })).toBeVisible();

    await page.click('button[type="submit"]');
    await page.waitForURL(/\/data_sources\/\d+/, { timeout: 10_000 });

    const rows = await queryRows<{ id: number }>(
      "SELECT id FROM data_sources WHERE name = $1",
      [dsName],
    );
    expect(rows.length).toBe(1);
    // node-pg returns bigint columns as strings — coerce before using as a JSON body field.
    const dsId = Number(rows[0].id);

    const q = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-csv-upload-query-${Date.now()}`,
      query: "SELECT * FROM sample",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');
      // The CSV connector ignores the query text and returns the whole file.
      await expect(page.locator("td").filter({ hasText: "alice" })).toBeVisible({
        timeout: 20_000,
      });
      await expect(page.locator("td").filter({ hasText: "30" })).toBeVisible();
      await expect(page.locator("td").filter({ hasText: "bob" })).toBeVisible();
      await expect(page.locator("td").filter({ hasText: "40" })).toBeVisible();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(dsId).catch(() => {});
    }
  } finally {
    await ctx.close();
  }
});
