/** E2E: query power features — CSV download, version history/revert, fork, favorites. */
import { readFileSync } from "node:fs";
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, withTargetDs } from "./helpers/api";
import { clickActionBarItem } from "./helpers/actions";

const BACKEND_URL = process.env.BACKEND_URL ?? "http://localhost:8080";

async function favoriteQueryIds(api: E2EApiClient): Promise<number[]> {
  const favs = await api.rawFetch<Array<{ id: number }>>("/api/queries/favorites");
  return favs.map((q) => q.id);
}

test("execute query then download results as CSV → valid header and row", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `csv-download-${Date.now()}`,
      query: "SELECT 42 AS answer",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 20_000,
      });

      // toBeVisible() misses an ancestor collapsing to zero height via flex overflow; measure a
      // real layout box at the default 720px viewport.
      const resultsPaneHeight = await page
        .getByTestId("query-results-pane")
        .evaluate((el) => el.clientHeight);
      expect(resultsPaneHeight).toBeGreaterThan(100);

      await page.click('button:has-text("Export")');
      const [download] = await Promise.all([
        page.waitForEvent("download"),
        page.click('button:has-text("Download CSV")'),
      ]);

      expect(download.suggestedFilename()).toMatch(/query-result-\d+\.csv$/);

      const filePath = await download.path();
      if (!filePath) throw new Error("download path is null");
      const content = readFileSync(filePath, "utf8");
      const lines = content.trim().split(/\r?\n/);
      expect(lines[0].toLowerCase()).toContain("answer");
      expect(lines[1]).toContain("42");
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("results grid: numeric columns are grouped, right-aligned, and the type badge is hidden from the a11y tree", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `numeric-fmt-${Date.now()}`,
      query: "SELECT 6853680.00::numeric(14,2) AS revenue, 42 AS orders, 'x' AS label",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');

      const revenueCell = page.locator("td", { hasText: "6,853,680.00" });
      await expect(revenueCell).toBeVisible({ timeout: 20_000 });
      await expect(revenueCell).toHaveCSS("text-align", "right");

      const labelCell = page.locator("td", { hasText: /^x$/ });
      await expect(labelCell).toHaveCSS("text-align", "start");

      // The "float" type badge is excluded from the a11y tree so it doesn't join the header's
      // accessible name.
      const revenueHeader = page.locator("th", { hasText: "revenue" });
      const badge = revenueHeader.locator('span[aria-hidden="true"]', { hasText: "float" });
      await expect(badge).toBeVisible();
      await expect(page.getByRole("button", { name: "revenuefloat" })).toHaveCount(0);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("results grid: sticky header, and fade affordances appear when content overflows", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    // 20 columns overflow horizontally, 14 rows overflow vertically (page size 15), no scrolling
    // needed.
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `results-overflow-${Date.now()}`,
      query: `SELECT i AS c1, i AS c2, i AS c3, i AS c4, i AS c5, i AS c6, i AS c7, i AS c8,
                     i AS c9, i AS c10, i AS c11, i AS c12, i AS c13, i AS c14, i AS c15,
                     i AS c16, i AS c17, i AS c18, i AS c19, i AS c20
               FROM generate_series(1, 14) AS i`,
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');
      await expect(page.locator("td").filter({ hasText: /^1$/ }).first()).toBeVisible({ timeout: 20_000 });

      const thead = page.locator("thead").first();
      await expect(thead).toHaveCSS("position", "sticky");

      // Rows overflow vertically: the bottom fade must be visible without scrolling.
      await expect(page.locator('[class*="bg-gradient-to-t"]').first()).toBeVisible();

      await expect(page.locator('[class*="bg-gradient-to-l"]').first()).toBeVisible();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("version history: 3 versions listed after 2 publishes; restoring v1 resets SQL + appends v4", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    // Only publish or restore appends a version (a draft edit records nothing), so 3 versions need
    // 3 publishes, each after an edit.
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `ver-hist-${Date.now()}`,
      query: "SELECT 1 AS v1",
    });
    try {
      await api.publishQuery(q.id); // v1

      await api.rawFetch(`/api/queries/${q.id}`, {
        method: "POST",
        body: JSON.stringify({ query: "SELECT 2 AS v2" }),
      });
      await api.publishQuery(q.id); // v2

      await api.rawFetch(`/api/queries/${q.id}`, {
        method: "POST",
        body: JSON.stringify({ query: "SELECT 3 AS v3" }),
      });
      await api.publishQuery(q.id); // v3

      await page.goto(`/queries/${q.id}`);

      // History is behind the "More actions" overflow menu at this viewport.
      await clickActionBarItem(page, "History");

      await expect(page.getByText(/3 version/i)).toBeVisible({ timeout: 8000 });

      await expect(page.locator("text=Version 1")).toBeVisible({ timeout: 5000 });
      await expect(page.locator("text=Version 2")).toBeVisible({ timeout: 5000 });
      await expect(page.locator("text=Version 3")).toBeVisible({ timeout: 5000 });

      // Two "Restore" buttons (v3 is current). Restore resets the draft AND appends a published
      // version; "Load into the draft" only does the former.
      const restoreButtons = page.locator('button[title="Restore — make this version the published version"]');
      await expect(restoreButtons).toHaveCount(2, { timeout: 5000 });

      await restoreButtons.last().click();

      await expect(page.getByText(/4 version/i)).toBeVisible({ timeout: 10_000 });

      await expect(page.locator(".cm-content")).toContainText("SELECT 1 AS v1", {
        timeout: 8000,
      });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("fork a query → navigates to new 'Copy of' draft owned by current user", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  let forkedId: number | null = null;

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `fork-src-${Date.now()}`,
      query: "SELECT 99 AS forked_val",
    });
    try {
      await page.goto(`/queries/${q.id}`);

      const origPath = new URL(page.url()).pathname;
      await clickActionBarItem(page, "Fork");

      await page.waitForURL((url) => url.pathname !== origPath, { timeout: 10_000 });

      const match = page.url().match(/\/queries\/(\d+)/);
      expect(match).not.toBeNull();
      const newId = Number(match![1]);
      expect(newId).not.toBe(q.id);
      forkedId = newId;

      await expect(page.locator("#query-name")).toHaveValue(/Copy of /i, { timeout: 8000 });

      // exact match: a docs link's sr-only text also contains "Draft".
      await expect(page.getByText("Draft", { exact: true })).toBeVisible({ timeout: 5000 });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  if (forkedId != null) {
    await api.deleteQuery(forkedId).catch(() => {});
  }

  await ctx.close();
});

test("star query in editor → appears in favorites; unstar → removed", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `star-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);

      // Star is in the "More actions" overflow menu; its label flips Star -> Unstar.
      await clickActionBarItem(page, "Star");

      await expect
        .poll(() => favoriteQueryIds(api), { timeout: 8000 })
        .toContain(q.id);

      await clickActionBarItem(page, "Unstar");

      await expect
        .poll(() => favoriteQueryIds(api), { timeout: 8000 })
        .not.toContain(q.id);
    } finally {
      await api.rawFetch(`/api/queries/${q.id}/favorite`, { method: "DELETE" })
        .catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("CSV download with session auth → content-type text/csv with correct data", async () => {
  const api = await loginAsAdmin();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `csv-api-${Date.now()}`,
      query: "SELECT 'hello' AS greeting, 42 AS num",
    });
    try {
      const resultId = await executeAndWait(api, q.id);

      type ApiPrivate = { sessionCookie: string };
      const cookie = (api as unknown as ApiPrivate).sessionCookie;
      const csvRes = await fetch(`${BACKEND_URL}/api/query_results/${resultId}.csv`, {
        headers: { Cookie: cookie },
      });
      expect(csvRes.status).toBe(200);
      expect(csvRes.headers.get("content-type") ?? "").toContain("text/csv");

      const text = await csvRes.text();
      const lines = text.trim().split(/\r?\n/);
      expect(lines[0].toLowerCase()).toMatch(/greeting/);
      expect(lines[0].toLowerCase()).toMatch(/num/);
      expect(lines[1]).toContain("hello");
      expect(lines[1]).toContain("42");
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });
});

test("a cell starting with a formula character is escaped in the downloaded CSV", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `formula-csv-${Date.now()}`,
      query: "SELECT '=1+1' AS cell",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');
      await expect(page.locator("td", { hasText: "=1+1" })).toBeVisible({ timeout: 20_000 });

      await page.click('button:has-text("Export")');
      const [download] = await Promise.all([
        page.waitForEvent("download"),
        page.click('button:has-text("Download CSV")'),
      ]);

      const filePath = await download.path();
      if (!filePath) throw new Error("download path is null");
      const content = readFileSync(filePath, "utf8");

      // A leading ' neutralizes =/+/-/@ cells for Excel/Sheets; the raw value must not appear on
      // its own.
      expect(content).toContain("'=1+1");
      expect(content).not.toMatch(/(^|[,\r\n])=1\+1/);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});
