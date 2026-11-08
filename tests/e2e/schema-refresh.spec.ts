/**
 * E2E: schema browser async refresh. `GET /api/data_sources/{id}/schema` is faked so each state
 * (first load, refreshing, flip, failure) is deterministic; the source itself is a real data
 * source.
 */
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { type E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";

const table = (name: string) => ({ name, columns: [{ name: "id", type_name: "int" }] });

interface Fake {
  tables: string[];
  refreshing: boolean;
  refresh_error: string | null;
}

async function setup(browser: import("@playwright/test").Browser) {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-schema-refresh-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  return { api, ds, ctx, page };
}

async function teardown(api: E2EApiClient, dsId: number, ctx: { close(): Promise<void> }) {
  await api.deleteDataSource(dsId).catch(() => {});
  await ctx.close();
}

/** Serves the fake state; `refresh=true` requests are recorded and flip the state to refreshing. */
async function fakeSchema(page: Page, dsId: number, state: Fake) {
  const calls = { refresh: 0, plain: 0 };
  await page.route(new RegExp(`/api/data_sources/${dsId}/schema\\?`), (route) => {
    if (new URL(route.request().url()).searchParams.get("refresh") === "true") {
      calls.refresh += 1;
      state.refreshing = true;
      state.refresh_error = null;
    } else {
      calls.plain += 1;
    }
    return route.fulfill({
      json: {
        schema: { tables: state.tables.map(table) },
        refreshing: state.refreshing,
        refresh_error: state.refresh_error,
      },
    });
  });
  return calls;
}

test("refresh disables the button, shows progress, and the new tables arrive when the job flips", async ({
  browser,
}) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    const state: Fake = { tables: ["orders"], refreshing: false, refresh_error: null };
    const calls = await fakeSchema(page, ds.id, state);
    await page.goto(`/data_sources/${ds.id}`);

    await expect(page.getByRole("button", { name: "orders", exact: true })).toBeVisible();
    const refresh = page.getByTestId("schema-refresh");
    await expect(refresh).toBeEnabled();
    await refresh.click();

    await expect(refresh).toBeDisabled();
    await expect(page.getByTestId("schema-refreshing")).toBeVisible();
    await expect(page.getByRole("button", { name: "orders", exact: true })).toBeVisible();
    expect(calls.refresh).toBe(1);

    state.tables = ["orders", "invoices"];
    state.refreshing = false;
    await expect(page.getByRole("button", { name: "invoices", exact: true })).toBeVisible({ timeout: 8000 });
    await expect(refresh).toBeEnabled();
    await expect(page.getByTestId("schema-refreshing")).toHaveCount(0);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("first load with nothing cached shows the loading line, then the tables", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    const state: Fake = { tables: [], refreshing: true, refresh_error: null };
    await fakeSchema(page, ds.id, state);
    await page.goto(`/data_sources/${ds.id}`);

    await expect(page.getByTestId("schema-first-load")).toBeVisible();
    await expect(page.getByText("No tables found.")).toHaveCount(0);

    state.tables = ["customers"];
    state.refreshing = false;
    await expect(page.getByRole("button", { name: "customers", exact: true })).toBeVisible({ timeout: 8000 });
    await expect(page.getByTestId("schema-first-load")).toHaveCount(0);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("a failed refresh shows the error, keeps old tables, and clears when the next refresh starts", async ({
  browser,
}) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    const state: Fake = { tables: ["orders"], refreshing: false, refresh_error: "connection refused" };
    await fakeSchema(page, ds.id, state);
    await page.goto(`/data_sources/${ds.id}`);

    await expect(page.getByTestId("schema-refresh-error")).toContainText(
      "Couldn't refresh the schema: connection refused",
    );
    await expect(page.getByRole("button", { name: "orders", exact: true })).toBeVisible();

    await page.getByTestId("schema-refresh").click();
    await expect(page.getByTestId("schema-refresh-error")).toHaveCount(0);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});
