// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows, deleteRows } from "./helpers/db";

async function withQuery(
  api: E2EApiClient,
  sql: string,
  fn: (ctx: { dsId: number; queryId: number }) => Promise<void>,
) {
  const stamp = Date.now() + Math.random();
  const ds = await api.createDataSource({
    name: `e2e-viz-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  let queryId: number | undefined;
  try {
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-viz-q-${stamp}`,
      query: sql,
    });
    queryId = q.id;
    await fn({ dsId: ds.id, queryId: q.id });
  } finally {
    if (queryId) await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

async function openAndExecute(page: import("@playwright/test").Page, queryId: number) {
  await page.goto(`/queries/${queryId}`);
  await page.getByRole("button", { name: /^(Execute|Running…)$/ }).click();
  await expect(page.getByRole("button", { name: "Results" })).toBeVisible({ timeout: 15_000 });
}

async function addVisualization(page: import("@playwright/test").Page, type: string, name: string) {
  await page.getByRole("button", { name: /add visualization/i }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible();
  await dialog.getByRole("combobox").click();
  await page.getByRole("option", { name: type, exact: true }).click();
  await dialog.getByPlaceholder("My visualization").fill(name);
  await dialog.getByRole("button", { name: "Add" }).click();
  await expect(dialog).not.toBeVisible();
  await expect(page.getByRole("button", { name }).first()).toBeVisible({ timeout: 8000 });
}


test("create a Table visualization → renders real rows immediately (default options)", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20)) AS t(day, visits)",
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      await addVisualization(page, "Table", `e2e-table-${Date.now()}`);
      await expect(page.locator("td").filter({ hasText: /^Mon$/ })).toBeVisible();
      await expect(page.locator("td").filter({ hasText: /^20$/ })).toBeVisible();
    },
  );

  await ctx.close();
});

test("create a Chart visualization, configure it (Stacking dropdown never crashes) → bars render", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20), ('Wed', 15)) AS t(day, visits)",
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      const name = `e2e-chart-${Date.now()}`;
      await addVisualization(page, "Chart", name);
      await page.getByRole("button", { name }).click();

      await page.getByTitle("Edit visualization").click();
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible();

      const combos = dialog.getByRole("combobox");
      await combos.nth(0).click();
      await page.getByRole("option", { name: "Bar", exact: true }).click();
      await combos.nth(1).click();
      await page.getByRole("option", { name: "day", exact: true }).click();
      await dialog.getByRole("checkbox", { name: "visits" }).check();
      // Index 4: "Group by" / "Combine rows" comboboxes sit between X-axis and Stacking.
      await combos.nth(4).click();
      const stackingOptions = page.getByRole("listbox");
      await expect(stackingOptions.getByRole("option")).toHaveCount(3);
      await stackingOptions.getByRole("option", { name: "Stacked", exact: true }).click();

      await dialog.getByRole("button", { name: "Save" }).click();
      await expect(dialog).not.toBeVisible();

      await expect(page.locator(".recharts-bar-rectangle").first()).toBeVisible({ timeout: 8000 });
    },
  );

  await ctx.close();
});

test("a fresh chart never invents a phantom series from a string column", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('East', 100), ('West', 200)) AS t(region, net_revenue)",
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      const name = `e2e-phantom-series-${Date.now()}`;
      // Bare defaultOptions (no xColumn/yColumns saved), like a fresh "Add Visualization".
      await addVisualization(page, "Chart", name);
      await page.getByRole("button", { name }).click();

      // "region" must never get a mark or legend entry.
      await expect(page.getByText("net_revenue", { exact: true })).toBeVisible();
      await expect(page.getByText("region", { exact: true })).not.toBeVisible();
      await expect(page.locator(".recharts-bar")).toHaveCount(1);
      await expect(page.locator(".recharts-bar-rectangle")).toHaveCount(2);
    },
  );

  await ctx.close();
});

test("chart editor pre-fills the rendered columns and only offers numeric Y-axis columns", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10, 'ok'), ('Tue', 20, 'ok')) AS t(day, visits, status)",
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      const name = `e2e-chart-sync-${Date.now()}`;
      await addVisualization(page, "Chart", name);
      await page.getByRole("button", { name }).click();
      await page.getByTitle("Edit visualization").click();

      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible();

      const combos = dialog.getByRole("combobox");
      await expect(combos.nth(1)).toHaveText("day");
      await expect(dialog.getByRole("checkbox", { name: "visits" })).toBeChecked();

      await expect(dialog.getByRole("checkbox", { name: "status" })).toHaveCount(0);
    },
  );

  await ctx.close();
});

test("Table visualization formats a timestamp the same human-readable way as the Results tab", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT '2024-01-15T10:30:00Z'::timestamptz AS created_at", async ({ queryId }) => {
    await openAndExecute(page, queryId);

    // The SQL editor pane contains the literal ISO text, so scope to table cells.
    await expect(page.locator("td").filter({ hasText: "2024-01-15T10:30:00" })).toHaveCount(0);
    await expect(page.locator("td").filter({ hasText: "2024" }).first()).toBeVisible();

    const name = `e2e-table-dates-${Date.now()}`;
    await addVisualization(page, "Table", name);
    await page.getByRole("button", { name }).click();

    await expect(page.locator("td").filter({ hasText: "2024-01-15T10:30:00" })).toHaveCount(0);
    await expect(page.locator("td").filter({ hasText: "2024" }).first()).toBeVisible();
  });

  await ctx.close();
});

test("create a Counter visualization (Target column dropdown never crashes) → shows $42 and roundtrips options", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT 42 AS total", async ({ queryId }) => {
    await openAndExecute(page, queryId);
    const name = `e2e-counter-${Date.now()}`;
    await addVisualization(page, "Counter", name);
    await page.getByRole("button", { name }).click();
    // Not the SQL editor, which also contains "42".
    await expect(page.getByTestId("counter-value")).toHaveText("42");

    await page.getByTitle("Edit visualization").click();
    const dialog = page.locator("[role=dialog]");
    await dialog.locator('input[value=""]').first().fill("$"); // Prefix field
    await dialog.getByRole("combobox").last().click();
    await expect(page.getByRole("option", { name: "None", exact: true })).toBeVisible();
    await page.getByRole("option", { name: "None", exact: true }).click();
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).not.toBeVisible();

    await expect(page.getByTestId("counter-value")).toHaveText("$42");

    // VizPanel/result live in page state, so re-execute after reload.
    await page.reload();
    await openAndExecute(page, queryId);
    await page.getByRole("button", { name }).click();
    await expect(page.getByTestId("counter-value")).toHaveText("$42");

    const vizs = await api.listVisualizations(queryId);
    const viz = vizs.find((v) => v.name === name)!;
    expect(viz.options.stringPrefix).toBe("$");
  });

  await ctx.close();
});

test("create a Funnel visualization → percentages render as 100%/40%/12%, no NaN", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Visited', 500), ('Signed up', 200), ('Purchased', 60)) AS t(stage, count)",
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      const name = `e2e-funnel-${Date.now()}`;
      await addVisualization(page, "Funnel", name);
      await page.getByRole("button", { name }).click();

      await page.getByTitle("Edit visualization").click();
      const dialog = page.locator("[role=dialog]");
      const combos = dialog.getByRole("combobox");
      await combos.nth(0).click();
      await page.getByRole("option", { name: "stage", exact: true }).click();
      await combos.nth(1).click();
      await page.getByRole("option", { name: "count", exact: true }).click();
      await dialog.getByRole("button", { name: "Save" }).click();
      await expect(dialog).not.toBeVisible();

      await expect(page.getByText("100%", { exact: true })).toBeVisible();
      await expect(page.getByText("40%", { exact: true })).toBeVisible();
      await expect(page.getByText("12%", { exact: true })).toBeVisible();
      await expect(page.getByText("NaN")).toHaveCount(0);
    },
  );

  await ctx.close();
});

test("Postgres NUMERIC (via ::numeric GROUP BY) renders a real chart line, no NaN anywhere", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  // SUM(amount::numeric) returns NUMERIC, serialized as a JSON string; ChartRenderer must coerce.
  await withQuery(
    api,
    `SELECT day, SUM(amount::numeric) AS total FROM
       (VALUES ('Mon', 10.5), ('Mon', 2.5), ('Tue', 7.25)) AS t(day, amount)
     GROUP BY day ORDER BY day`,
    async ({ queryId }) => {
      await openAndExecute(page, queryId);
      const name = `e2e-numeric-chart-${Date.now()}`;
      await addVisualization(page, "Chart", name);
      await page.getByRole("button", { name }).click();

      await page.getByTitle("Edit visualization").click();
      const dialog = page.locator("[role=dialog]");
      const combos = dialog.getByRole("combobox");
      await combos.nth(0).click();
      await page.getByRole("option", { name: "Line", exact: true }).click();
      await combos.nth(1).click();
      await page.getByRole("option", { name: "day", exact: true }).click();
      await dialog.getByRole("checkbox", { name: "total" }).check();
      await dialog.getByRole("button", { name: "Save" }).click();
      await expect(dialog).not.toBeVisible();

      await expect(page.locator(".recharts-line-curve").first()).toBeVisible({ timeout: 8000 });
      const d = await page.locator(".recharts-line-curve").first().getAttribute("d");
      expect(d).not.toContain("NaN");
      await expect(page.getByText("NaN")).toHaveCount(0);
    },
  );

  await ctx.close();
});

test("empty result set: Table/Counter/Chart/Funnel all render a no-crash placeholder", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT 'x' AS label, 1 AS val WHERE false", async ({ queryId }) => {
    await openAndExecute(page, queryId);

    const tableName = `e2e-empty-table-${Date.now()}`;
    await addVisualization(page, "Table", tableName);
    await page.getByRole("button", { name: tableName }).click();
    await expect(page.getByText(/no rows returned/i)).toBeVisible();

    const counterName = `e2e-empty-counter-${Date.now()}`;
    await addVisualization(page, "Counter", counterName);
    await page.getByRole("button", { name: counterName }).click();
    await expect(page.getByText("—", { exact: true })).toBeVisible();

    const chartName = `e2e-empty-chart-${Date.now()}`;
    await addVisualization(page, "Chart", chartName);
    await page.getByRole("button", { name: chartName }).click();
    // A zero-row result has zero columns, so the "no columns" empty state renders, not "No rows to
    // plot".
    await expect(page.getByText("No columns in this result")).toBeVisible();
  });

  await ctx.close();
});

test("deleting a visualization (confirm dialog) cascades to its dashboard widget", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT 42 AS total", async ({ queryId }) => {
    const viz = await api.createVisualization({
      query_id: queryId,
      type: "COUNTER",
      name: `e2e-cascade-${Date.now()}`,
      options: { columnName: "total" },
    });
    await executeAndWait(api, queryId);
    await api.publishQuery(queryId);
    const dash = await api.createDashboard(`e2e-viz-cascade-${Date.now()}`);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });

      await openAndExecute(page, queryId);
      await page.getByRole("button", { name: viz.name }).click();
      await page.getByTitle("Delete visualization").click();
      const confirm = page.getByRole("dialog");
      await confirm.getByRole("button", { name: "Delete", exact: true }).click();
      // The open modal aria-hides the tab; wait for the delete to finish.
      await expect(confirm).not.toBeVisible();
      await expect(page.getByRole("button", { name: viz.name })).not.toBeVisible();

      const widgetRows = await queryRows<{ id: string }>(
        "SELECT id FROM widgets WHERE dashboard_id = $1",
        [dash.id],
      );
      expect(widgetRows.length).toBe(0);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.getByText("Add your first widget")).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });

  await ctx.close();
});

test("renaming a visualization from its Edit dialog persists across reload", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT 42 AS total", async ({ queryId }) => {
    const stamp = Date.now();
    const viz = await api.createVisualization({
      query_id: queryId,
      type: "COUNTER",
      name: `e2e-edit-before-${stamp}`,
      options: { columnName: "total" },
    });
    const newName = `e2e-edit-after-${stamp}`;

    await executeAndWait(api, queryId);
    await openAndExecute(page, queryId);
    await page.getByRole("button", { name: viz.name }).click();
    await page.getByTitle("Edit visualization").click();
    const dialog = page.getByRole("dialog");
    await dialog.locator("#edit-viz-name").fill(newName);
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).not.toBeVisible();
    await expect(page.getByRole("button", { name: newName })).toBeVisible();

    await openAndExecute(page, queryId);
    await expect(page.getByRole("button", { name: newName })).toBeVisible();
    await expect(page.getByRole("button", { name: viz.name })).toHaveCount(0);
  });

  await ctx.close();
});

test("renaming a visualization updates the dashboard widget's title", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(api, "SELECT 42 AS total", async ({ queryId }) => {
    const viz = await api.createVisualization({
      query_id: queryId,
      type: "COUNTER",
      name: `e2e-before-rename-${Date.now()}`,
      options: { columnName: "total" },
    });
    await executeAndWait(api, queryId);
    await api.publishQuery(queryId);
    const dash = await api.createDashboard(`e2e-rename-${Date.now()}`);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      // A fresh load shows the published snapshot, so publish first.
      await api.publishDashboard(dash.slug);

      const newName = `e2e-after-rename-${Date.now()}`;
      await api.updateVisualization(viz.id, { name: newName });

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.getByText(newName)).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });

  await ctx.close();
});

test("dashboard widgets render both a chart (svg marks) and a counter", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20)) AS t(day, visits)",
    async ({ queryId }) => {
      const chartViz = await api.createVisualization({
        query_id: queryId,
        type: "CHART",
        name: `e2e-widget-chart-${Date.now()}`,
        options: { globalSeriesType: "bar", xColumn: "day", yColumns: ["visits"] },
      });
      await executeAndWait(api, queryId);
      await api.publishQuery(queryId);

      await withQuery(api, "SELECT 42 AS total", async ({ queryId: counterQueryId }) => {
        const counterViz = await api.createVisualization({
          query_id: counterQueryId,
          type: "COUNTER",
          name: `e2e-widget-counter-${Date.now()}`,
          options: { columnName: "total" },
        });
        await executeAndWait(api, counterQueryId);
        await api.publishQuery(counterQueryId);

        const dash = await api.createDashboard(`e2e-widgets-render-${Date.now()}`);
        try {
          await api.createWidget({
            dashboard_id: dash.id,
            visualization_id: chartViz.id,
            text: null,
            options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
          });
          await api.createWidget({
            dashboard_id: dash.id,
            visualization_id: counterViz.id,
            text: null,
            options: { position: { col: 4, row: 0, sizeX: 2, sizeY: 3 } },
          });
          // A fresh load shows the published snapshot, so publish first.
          await api.publishDashboard(dash.slug);

          await page.goto(`/dashboard/${dash.slug}`);
          await expect(page.locator(".recharts-bar-rectangle").first()).toBeVisible({ timeout: 15_000 });
          await expect(page.getByText("42", { exact: true })).toBeVisible();
        } finally {
          await api.deleteDashboard(dash.slug).catch(() => {});
        }
      });
    },
  );

  await ctx.close();
});

test("public dashboard renders a chart's svg marks anonymously", async ({ browser }) => {
  const api = await loginAsAdmin();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20)) AS t(day, visits)",
    async ({ queryId }) => {
      const viz = await api.createVisualization({
        query_id: queryId,
        type: "CHART",
        name: `e2e-public-chart-${Date.now()}`,
        options: { globalSeriesType: "bar", xColumn: "day", yColumns: ["visits"] },
      });
      await executeAndWait(api, queryId);
      await api.publishQuery(queryId);

      const dash = await api.createDashboard(`e2e-public-chart-dash-${Date.now()}`);
      try {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: viz.id,
          text: null,
          options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
        });
        await api.publishDashboard(dash.slug);
        const { public_url } = await api.shareDashboard(dash.slug);

        const incognito = await browser.newContext();
        const publicPage = await incognito.newPage();
        await publicPage.goto(public_url);
        await expect(publicPage.getByText(dash.name)).toBeVisible({ timeout: 8000 });
        await expect(publicPage.locator(".recharts-bar-rectangle").first()).toBeVisible({ timeout: 15_000 });
        await incognito.close();
      } finally {
        await api.revokeShareDashboard(dash.slug).catch(() => {});
        await api.deleteDashboard(dash.slug).catch(() => {});
      }
    },
  );
});

// jsdom can't cover this: the mobile stack wrapper needs a real height or the chart measures 0x0 at
// phone widths.
test("dashboard widget chart renders at non-zero height on mobile (390px), same as desktop", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20), ('Wed', 15)) AS t(day, visits)",
    async ({ queryId }) => {
      const viz = await api.createVisualization({
        query_id: queryId,
        type: "CHART",
        name: `e2e-mobile-height-${Date.now()}`,
        options: { globalSeriesType: "bar", xColumn: "day", yColumns: ["visits"] },
      });
      await executeAndWait(api, queryId);
      await api.publishQuery(queryId);

      const dash = await api.createDashboard(`e2e-mobile-height-${Date.now()}`);
      try {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: viz.id,
          text: null,
          options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
        });
        await api.publishDashboard(dash.slug);

        for (const viewport of [{ width: 390, height: 844 }, { width: 1440, height: 900 }]) {
          await page.setViewportSize(viewport);
          await page.goto(`/dashboard/${dash.slug}`);
          if (viewport.width === 390) {
            await expect(page.getByTestId("dashboard-stack")).toBeVisible({ timeout: 15_000 });
          }
          const surface = page.locator(".recharts-responsive-container").first();
          await expect(surface).toBeVisible({ timeout: 15_000 });
          // Floor of 200px, not >0, so a 1px sliver fails.
          await expect
            .poll(async () => (await surface.boundingBox())?.height ?? 0, { timeout: 15_000 })
            .toBeGreaterThan(50);
        }
      } finally {
        await api.deleteDashboard(dash.slug).catch(() => {});
      }
    },
  );

  await ctx.close();
});

test("public dashboard embed widget renders at non-zero height on mobile (390px), same as desktop", async ({
  browser,
}) => {
  const api = await loginAsAdmin();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20)) AS t(day, visits)",
    async ({ queryId }) => {
      const viz = await api.createVisualization({
        query_id: queryId,
        type: "CHART",
        name: `e2e-mobile-embed-${Date.now()}`,
        options: { globalSeriesType: "bar", xColumn: "day", yColumns: ["visits"] },
      });
      await executeAndWait(api, queryId);
      await api.publishQuery(queryId);

      const dash = await api.createDashboard(`e2e-mobile-embed-${Date.now()}`);
      try {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: viz.id,
          text: null,
          options: { position: { col: 0, row: 0, sizeX: 4, sizeY: 3 } },
        });
        await api.publishDashboard(dash.slug);

        const embed = await api.rawFetch<{ id: number; public_url: string }>("/api/embeds", {
          method: "POST",
          body: JSON.stringify({
            object_type: "dashboard",
            object_id: dash.id,
            anonymous: true,
            locked_params: {},
          }),
        });
        try {
          const incognito = await browser.newContext();
          const publicPage = await incognito.newPage();
          for (const viewport of [{ width: 390, height: 844 }, { width: 1440, height: 900 }]) {
            await publicPage.setViewportSize(viewport);
            await publicPage.goto(embed.public_url);
            if (viewport.width === 390) {
              await expect(publicPage.getByTestId("dashboard-stack")).toBeVisible({ timeout: 15_000 });
            }
            const surface = publicPage.locator(".recharts-responsive-container").first();
            await expect(surface).toBeVisible({ timeout: 15_000 });
            await expect
              .poll(async () => (await surface.boundingBox())?.height ?? 0, { timeout: 15_000 })
              .toBeGreaterThan(50);
          }
          await incognito.close();
        } finally {
          await api.rawFetch(`/api/embeds/${embed.id}`, { method: "DELETE" }).catch(() => {});
        }
      } finally {
        await api.deleteDashboard(dash.slug).catch(() => {});
      }
    },
  );
});

// ChartRenderer's ResponsiveContainer uses height="100%"; without a sized ancestor a public embed
// collapses to 0 height.
test("public visualization embed (not a dashboard) renders a chart's svg marks at non-zero height", async ({
  browser,
}) => {
  const api = await loginAsAdmin();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('Mon', 10), ('Tue', 20)) AS t(day, visits)",
    async ({ queryId }) => {
      const viz = await api.createVisualization({
        query_id: queryId,
        type: "CHART",
        name: `e2e-public-viz-embed-${Date.now()}`,
        options: { globalSeriesType: "bar", xColumn: "day", yColumns: ["visits"] },
      });
      await executeAndWait(api, queryId);
      await api.publishQuery(queryId);

      const embed = await api.rawFetch<{ id: number; public_url: string }>("/api/embeds", {
        method: "POST",
        body: JSON.stringify({
          object_type: "visualization",
          object_id: viz.id,
          anonymous: true,
          locked_params: {},
        }),
      });
      try {
        const incognito = await browser.newContext();
        const publicPage = await incognito.newPage();
        await publicPage.goto(embed.public_url);
        const bar = publicPage.locator(".recharts-bar-rectangle").first();
        await expect(bar).toBeVisible({ timeout: 15_000 });
        // Poll: Recharts animates bar height from 0, so a one-shot boundingBox() can read a stale
        // value.
        await expect
          .poll(async () => (await bar.boundingBox())?.height ?? 0, { timeout: 15_000 })
          .toBeGreaterThan(0);
        await incognito.close();
      } finally {
        await api.rawFetch(`/api/embeds/${embed.id}`, { method: "DELETE" }).catch(() => {});
      }
    },
  );
});

test("dashboard param + previously-uncached query combo eventually renders (no stuck Running…)", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQuery(
    api,
    "SELECT * FROM (VALUES ('a', 1), ('b', 2)) AS t(label, val) WHERE label = '{{label}}'",
    async ({ queryId }) => {
      // {{param}} needs a saved definition in query.options.parameters, not just the SQL token.
      await api.rawFetch(`/api/queries/${queryId}`, {
        method: "POST",
        body: JSON.stringify({ options: { parameters: [{ name: "label", title: "label", type: "text" }] } }),
      });

      await api.publishQuery(queryId);

      const viz = await api.createVisualization({
        query_id: queryId,
        type: "TABLE",
        name: `e2e-param-widget-${Date.now()}`,
        options: {},
      });
      const dash = await api.createDashboard(`e2e-viz-param-${Date.now()}`);
      try {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: viz.id,
          text: null,
          options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
        });
        // A fresh load shows the published snapshot, so publish first.
        await api.publishDashboard(dash.slug);

        await page.goto(`/dashboard/${dash.slug}`);
        const paramInput = page.getByTestId("dashboard-param-label").locator("input");
        await expect(paramInput).toBeVisible({ timeout: 8000 });
        // A never-executed param value guarantees a cache miss, so the widget must poll the
        // returned job.
        await paramInput.fill("a");

        await expect(page.locator("td").filter({ hasText: /^1$/ })).toBeVisible({ timeout: 20_000 });
        await expect(page.getByText("Running…")).not.toBeVisible();
      } finally {
        await api.deleteDashboard(dash.slug).catch(() => {});
      }
    },
  );

  await ctx.close();
});

// Query-scoped api_keys row; no API endpoint mints one, so insert directly.
async function grantQueryApiKey(queryId: number, key: string) {
  const [org] = await queryRows<{ org_id: string }>("SELECT org_id FROM queries WHERE id = $1", [queryId]);
  // Hash must match arcodash_core::hash::hash_api_key (sha256).
  await queryRows(
    "INSERT INTO api_keys (org_id, object_type, object_id, api_key_hash) VALUES ($1, 'query', $2, encode(sha256($3::bytea), 'hex'))",
    [org.org_id, queryId, key],
  );
}

test("the api_key embed endpoint returns 404 even with a valid query key (embeds are token-based)", async () => {
  const api = await loginAsAdmin();

  await withQuery(api, "SELECT 42 AS total", async ({ queryId }) => {
    const viz = await api.createVisualization({
      query_id: queryId,
      type: "COUNTER",
      name: `e2e-embed-${Date.now()}`,
      options: { columnName: "total" },
    });
    await executeAndWait(api, queryId);

    const key = `e2e-embed-key-${Date.now()}`;
    await grantQueryApiKey(queryId, key);
    try {
      expect(await api.getStatus(`/api/visualizations/${viz.id}/embed?api_key=${key}`)).toBe(404);
      expect(await api.getStatus(`/api/visualizations/${viz.id}/embed`)).toBe(404);
    } finally {
      await deleteRows("api_keys", "api_key_hash = encode(sha256($1::bytea), 'hex')", [key]);
    }
  });
});
