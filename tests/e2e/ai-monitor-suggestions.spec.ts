/** E2E: AI data-quality monitor suggestions, browser to database. The AI hop (POST
 * /api/data_sources/:id/suggest_monitors) is stubbed for determinism; everything downstream is
 * real. Requires: backend :8080, frontend :3000, target postgres :5434. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { type E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { getQuery, queryRows } from "./helpers/db";

/** Stub client_config so the card always renders, without a real provider. */
async function stubClientConfig(page: import("@playwright/test").Page) {
  await page.route("**/api/session", async (route) => {
    const res = await route.fetch();
    const json = await res.json();
    json.client_config = {
      ...(json.client_config ?? {}),
      ai_monitor_suggestions: true,
      ai_sql_generation: true,
    };
    await route.fulfill({ json });
  });
}

function rawSuggestion(overrides: Record<string, unknown>) {
  return {
    table: "orders",
    kind: "row_count",
    title: "Orders goes empty",
    rationale: "Alerts if orders has no rows at all.",
    sql: "SELECT COUNT(*) AS row_count FROM orders",
    column: "row_count",
    op: "<",
    value: 1,
    guessed_threshold: false,
    ...overrides,
  };
}

async function createTargetPgDataSource(api: E2EApiClient, name: string) {
  return api.createDataSource({ name, type: "pg", options: TARGET_DS_OPTIONS });
}

test("suggest → test → create produces a real published query, monitor, and self-subscription", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await createTargetPgDataSource(api, `e2e-dq-suggest-${Date.now()}`);
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await stubClientConfig(page);

  const checkTitle = `Orders row-count check ${Date.now()}`;
  await page.route(`**/api/data_sources/${ds.id}/suggest_monitors`, (route) =>
    route.fulfill({
      json: {
        status: "ok",
        suggestions: [rawSuggestion({ title: checkTitle })],
        covered: [],
        scanned_tables: 1,
        served_by: "openai",
      },
    }),
  );

  try {
    await page.goto(`/data_sources/${ds.id}`);

    const cardHeading = page.getByRole("heading", { name: "Data quality checks" });
    const schemaHeading = page.getByText("Schema", { exact: true });
    await expect(cardHeading).toBeVisible({ timeout: 10_000 });
    const cardBox = await cardHeading.boundingBox();
    const schemaBox = await schemaHeading.boundingBox();
    expect(cardBox && schemaBox && cardBox.y).toBeLessThan(schemaBox!.y);

    await page.getByRole("button", { name: "Suggest checks with AI" }).click();
    await expect(page.getByText(checkTitle)).toBeVisible({ timeout: 10_000 });
    // Scope by class: the schema browser's "orders" table button also matches plain text.
    await expect(page.locator("p.uppercase", { hasText: "orders" })).toBeVisible();

    // Test runs the SQL against the real target-postgres source.
    await page.getByRole("button", { name: "Test", exact: true }).click();
    await expect(page.getByText(/Row count is \d/)).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: "Create check" }).click();
    await expect(page.getByRole("link", { name: "View monitor" })).toBeVisible({ timeout: 10_000 });

    await expect(page.getByText(/1 check created\./)).toBeVisible();
    const teammatesLink = page.getByRole("link", { name: "Add teammates" });
    const href = await teammatesLink.getAttribute("href");
    expect(href).toMatch(/^\/monitors\/\d+$/);

    // bigint columns come back as strings from pg.
    const query = await getQuery(checkTitle);
    expect(query).not.toBeNull();
    expect(query!.published).toBe(true);

    const monitors = await queryRows<{ id: string; options: Record<string, unknown> }>(
      "SELECT id, options FROM monitors WHERE name = $1",
      [checkTitle],
    );
    expect(monitors.length).toBe(1);
    expect(monitors[0].options.column).toBe("row_count");
    expect(monitors[0].options.kind).toBe("baseline");
    expect(monitors[0].options.op).toBeUndefined();
    expect(monitors[0].options.value).toBeUndefined();

    const subs = await queryRows<{ destination_id: string | null }>(
      "SELECT destination_id FROM monitor_subscriptions WHERE monitor_id = $1",
      [Number(monitors[0].id)],
    );
    expect(subs.length).toBe(1);
    expect(subs[0].destination_id).toBeNull();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("batch: selecting two suggestions creates both and reports '2 of 2 checks created'", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await createTargetPgDataSource(api, `e2e-dq-batch-${Date.now()}`);
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await stubClientConfig(page);

  const stamp = Date.now();
  const titleA = `Orders batch check A ${stamp}`;
  const titleB = `Customers batch check B ${stamp}`;
  await page.route(`**/api/data_sources/${ds.id}/suggest_monitors`, (route) =>
    route.fulfill({
      json: {
        status: "ok",
        suggestions: [
          rawSuggestion({ table: "orders", title: titleA }),
          rawSuggestion({
            table: "customers",
            title: titleB,
            sql: "SELECT COUNT(*) AS row_count FROM customers",
          }),
        ],
        covered: [],
        scanned_tables: 2,
        served_by: "openai",
      },
    }),
  );

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await page.getByRole("button", { name: "Suggest checks with AI" }).click();
    await expect(page.getByText(titleA)).toBeVisible({ timeout: 10_000 });
    await expect(page.getByText(titleB)).toBeVisible();

    const checkboxes = page.getByRole("checkbox", { name: /^Select/ });
    await checkboxes.nth(0).click();
    await checkboxes.nth(1).click();

    await page.getByRole("button", { name: "Create 2 checks" }).click();
    await expect(page.getByText("2 of 2 checks created.")).toBeVisible({ timeout: 15_000 });

    const monitors = await queryRows<{ id: string }>(
      "SELECT id FROM monitors WHERE name IN ($1, $2)",
      [titleA, titleB],
    );
    expect(monitors.length).toBe(2);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("data source with more than 20 tables shows the scope picker and caps selection at 20", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await createTargetPgDataSource(api, `e2e-dq-scope-${Date.now()}`);
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await stubClientConfig(page);

  const tableNames = Array.from({ length: 21 }, (_, i) => `scope_table_${i}`);
  await page.route(`**/api/data_sources/${ds.id}/schema*`, (route) =>
    route.fulfill({
      json: {
        schema: {
          tables: tableNames.map((name) => ({ name, columns: [{ name: "id", type_name: "bigint" }] })),
        },
      },
    }),
  );

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await page.getByRole("button", { name: "Suggest checks with AI" }).click();

    await expect(page.getByText(/This data source has 21 tables/)).toBeVisible({ timeout: 10_000 });
    const runButton = page.getByRole("button", { name: /^Suggest checks for \d+ tables$/ });
    await expect(runButton).toBeDisabled();

    // The 21st stays disabled once the cap is hit, so it's never clicked.
    for (const name of tableNames.slice(0, 20)) {
      await page.getByRole("checkbox", { name, exact: true }).click();
    }
    await expect(page.getByRole("checkbox", { name: tableNames[20], exact: true })).toBeDisabled();
    await expect(page.getByRole("button", { name: "Suggest checks for 20 tables" })).toBeEnabled();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("a rate_limited response shows the destructive banner with a retry-after suffix", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await createTargetPgDataSource(api, `e2e-dq-ratelimit-${Date.now()}`);
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await stubClientConfig(page);

  await page.route(`**/api/data_sources/${ds.id}/suggest_monitors`, (route) =>
    route.fulfill({
      json: { status: "rate_limited", message: "Too many requests.", retry_after_secs: 34 },
    }),
  );

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await page.getByRole("button", { name: "Suggest checks with AI" }).click();
    await expect(page.getByText("Too many requests. (try again in 34s)")).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByRole("button", { name: "Try again" })).toBeVisible();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
