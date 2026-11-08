// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, withTargetDs } from "./helpers/api";
import { selectDataSource } from "./helpers/actions";

// Execute must run the query even when the SQL editor's autocomplete popup is still open, as it
// is right after typing a keyword or column prefix by hand.

const EXECUTE_REQUEST = /\/api\/(query_results|queries\/\d+\/results)$/;

async function typeSqlByHand(page: Page, sql: string, popup: "open" | "closed") {
  await page.locator(".cm-content").click();
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.type(sql, { delay: 25 });
  const tooltip = page.locator(".cm-tooltip-autocomplete");
  await expect(tooltip).toBeVisible();
  if (popup === "closed") {
    await page.keyboard.press("Escape");
    await expect(tooltip).toBeHidden();
  }
}

/** Clicks Execute and resolves once the run request has gone out, failing if it never does. */
async function executeAndExpectRun(page: Page) {
  const run = page.waitForRequest((r) => r.method() === "POST" && EXECUTE_REQUEST.test(r.url()), {
    timeout: 10_000,
  });
  await page.getByRole("button", { name: "Execute" }).click();
  await run;
}

for (const popup of ["open", "closed"] as const) {
  test(`Execute runs a new and a saved query with the autocomplete popup ${popup}`, async ({
    browser,
  }) => {
    test.setTimeout(120_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();

    await withTargetDs(api, async (_dsId, dsName) => {
      let queryId: number | null = null;
      try {
        await page.goto("/queries/new");
        await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });
        await page.fill("#query-name", `e2e-autocomplete-${Date.now()}`);
        await selectDataSource(page, dsName);
        await expect(page.getByRole("button", { name: "typed_table" })).toBeVisible();

        await typeSqlByHand(page, "SELECT int_col, text_col FROM typed_table ORDER BY 1 DESC", popup);
        await executeAndExpectRun(page);
        await page.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
        queryId = Number(page.url().match(/\/queries\/(\d+)/)![1]);
        await expect(page.getByText("Run the query to see results.")).toHaveCount(0);

        await typeSqlByHand(page, "SELECT text_col FROM typed_table ORDER BY text_col DESC", popup);
        await executeAndExpectRun(page);
      } finally {
        if (queryId !== null) await api.deleteQuery(queryId).catch(() => {});
      }
    });
  });
}
