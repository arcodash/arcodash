// typed_table (tests/e2e/seed/target-db.sql) has rows text_col='hello' (int_col=42) and text_col=''
// (int_col=0).
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { typeSql } from "./helpers/editor";

test("{{name}} parameter: configure → execute filters rows; changing value changes results", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-param-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-param-q-${Date.now()}`,
    query: "SELECT int_col, text_col FROM typed_table WHERE text_col = '{{name}}'",
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto(`/queries/${q.id}`);

    const configurator = page.getByTestId("param-name");
    await expect(configurator).toBeVisible({ timeout: 8000 });
    await configurator.getByRole("button", { name: "Add parameter" }).click();

    const paramInput = page.getByTestId("param-name").locator("input");
    await expect(paramInput).toBeVisible({ timeout: 5000 });

    await paramInput.fill("hello");
    await page.getByRole("button", { name: /^(Execute|Running…)$/ }).click();
    await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: /^0$/ })).not.toBeVisible();

    await paramInput.fill("");
    await page.getByRole("button", { name: /^(Execute|Running…)$/ }).click();
    await expect(page.locator("td").filter({ hasText: /^0$/ })).toBeVisible({ timeout: 15_000 });
    await expect(page.locator("td").filter({ hasText: /^42$/ })).not.toBeVisible();
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("a query-based dropdown whose source is still running shows Loading…, then gives up instead of polling forever", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-param-poll-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const source = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-param-poll-source-${Date.now()}`,
    query: "SELECT DISTINCT text_col AS name, text_col AS value FROM typed_table",
  });
  const main = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-param-poll-main-${Date.now()}`,
    query: "SELECT * FROM typed_table WHERE text_col = '{{dept}}'",
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    // Every poll returns "still enqueued", so the ladder runs for real without taking minutes.
    let hits = 0;
    await page.route(`**/api/queries/${main.id}/parameter_values*`, (route) => {
      hits += 1;
      return route.fulfill({
        status: 200,
        contentType: "application/json",
        body: JSON.stringify({ job: { id: "e2e-fake-job", status: 1, query_result_id: null, error: null } }),
      });
    });

    await page.goto(`/queries/${main.id}`);

    const configurator = page.getByTestId("param-dept");
    await expect(configurator).toBeVisible({ timeout: 8000 });
    await configurator.getByRole("combobox").click();
    await page.getByRole("option", { name: "Query-based Dropdown" }).click();
    await configurator.getByPlaceholder("e.g. 12").fill(String(source.id));
    await configurator.getByRole("button", { name: "Add parameter" }).click();

    const control = page.getByTestId("param-dept");
    await expect(control.getByRole("button", { name: "Loading…" })).toBeVisible({ timeout: 8000 });
    await expect(control.getByRole("combobox")).toHaveCount(0);

    await expect(control.getByRole("button", { name: "Options not ready" })).toBeVisible({
      timeout: 40_000,
    });
    const hitsAtGiveUp = hits;
    await page.waitForTimeout(8000);
    expect(hits).toBe(hitsAtGiveUp);
  } finally {
    await api.deleteQuery(main.id).catch(() => {});
    await api.deleteQuery(source.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("configuring a parameter while a SQL edit is still saving doesn't fabricate a conflict", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-param-race-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-param-race-q-${Date.now()}`,
    query: "SELECT int_col, text_col FROM typed_table",
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    // Delay every save so a SQL edit's autosave is still in flight when the parameter is
    // configured; the overlap must not send two versioned PATCHes (the loser would 409).
    await page.route(`**/api/queries/${q.id}`, async (route) => {
      if (route.request().method() === "POST") {
        await new Promise((r) => setTimeout(r, 1500));
      }
      await route.continue();
    });

    await page.goto(`/queries/${q.id}`);
    await expect(page.locator(".cm-content")).toBeVisible({ timeout: 8000 });

    // Wait for the actual request, not the "Saving…" label.
    const savePost = page.waitForRequest(
      (req) => req.url().includes(`/api/queries/${q.id}`) && req.method() === "POST",
    );
    await typeSql(page, "SELECT int_col, text_col FROM typed_table WHERE text_col = '{{foo}}'");

    await savePost;
    await expect(page.getByText("Saving…")).toBeVisible({ timeout: 3000 });

    const configurator = page.getByTestId("param-foo");
    await expect(configurator).toBeVisible({ timeout: 8000 });
    await configurator.getByRole("button", { name: "Add parameter" }).click();

    await expect(page.getByTestId("param-foo").locator("input")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByRole("status").filter({ hasText: "Saved" })).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(/changed by someone else/)).not.toBeVisible();

    await page.reload();
    await expect(page.getByTestId("param-foo").locator("input")).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(/changed by someone else/)).not.toBeVisible();
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
