// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, executeAndWait, E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows, deleteRows } from "./helpers/db";
import { publishWake } from "./helpers/valkey";
import { seedCustomPermissionUser, seededUserContext } from "./helpers/permissions";

// Fixture: target DS + published query (SELECT 42) + table viz; widget_service::create rejects an
// unpublished query's viz.
async function withQueryAndViz(
  api: E2EApiClient,
  fn: (ctx: { dsId: number; queryId: number; vizId: number; queryName: string }) => Promise<void>,
) {
  const stamp = Date.now();
  const ds = await api.createDataSource({
    name: `e2e-widget-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  let queryId: number | undefined;
  let vizId: number | undefined;
  const queryName = `e2e-widget-q-${stamp}`;
  try {
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: queryName,
      query: "SELECT 42 AS val",
    });
    queryId = q.id;
    await api.publishQuery(q.id);
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `e2e-widget-viz-${stamp}`,
      options: {},
    });
    vizId = viz.id;
    await fn({ dsId: ds.id, queryId: q.id, vizId: viz.id, queryName });
  } finally {
    if (vizId) await api.deleteVisualization(vizId).catch(() => {});
    if (queryId) await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

async function updateQuerySql(api: E2EApiClient, queryId: number, sql: string): Promise<void> {
  const full = await api.rawFetch<{ working_version: number }>(`/api/queries/${queryId}`);
  await api.rawFetch(`/api/queries/${queryId}`, {
    method: "POST",
    body: JSON.stringify({ query: sql, expected_version: full.working_version }),
  });
}

// Polls the runs table for the newest row's terminal status, nudging the deadline-scheduler's wake
// channel (the page shows no run-status text).
async function waitForRunRow(
  queryId: number,
  status: "success" | "failed",
  timeoutMs: number,
): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    await publishWake().catch(() => {});
    const rows = await queryRows<{ status: string }>(
      "SELECT status FROM runs WHERE entity_type = 'query' AND entity_id = $1 ORDER BY id DESC LIMIT 1",
      [queryId],
    );
    if (rows[0]?.status === status) return;
    await new Promise((r) => setTimeout(r, 1000));
  }
  throw new Error(`no "${status}" run appeared for query ${queryId} within ${timeoutMs}ms`);
}


test("add widget via UI → widget renders query result data", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQueryAndViz(api, async ({ queryId, queryName }) => {
    await executeAndWait(api, queryId);

    const dash = await api.createDashboard(`e2e-add-widget-${Date.now()}`);
    try {
      await page.goto(`/dashboard/${dash.slug}`);

      await page.click('button:has-text("Edit")');
      await page.click('button:has-text("Add Widget")');
      await expect(page.locator("[role=dialog]")).toBeVisible({ timeout: 8000 });

      // Single-item Radix listbox: a positioning regression renders the item over the trigger like
      // a made selection, so assert committed state.
      const querySelect = page.getByTestId("add-widget-query-select");
      await querySelect.click();
      await page.getByRole("option", { name: queryName }).click();
      await expect(querySelect).toHaveAttribute("aria-expanded", "false");
      await expect(querySelect).toContainText(queryName);

      const vizSelect = page.getByTestId("add-widget-viz-select");
      await expect(vizSelect).toBeVisible({ timeout: 8000 });
      await vizSelect.click();
      const vizOption = page.getByRole("option").first();
      const vizOptionText = (await vizOption.textContent())?.trim();
      await vizOption.click();
      await expect(vizSelect).toHaveAttribute("aria-expanded", "false");
      if (vizOptionText) await expect(vizSelect).toContainText(vizOptionText);

      const addButton = page.locator('[role=dialog] button:has-text("Add")');
      await expect(addButton).toBeEnabled();
      await addButton.click();
      await expect(page.locator("[role=dialog]")).not.toBeVisible({ timeout: 8000 });

      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });

      const rows = await queryRows<{ id: string }>(
        "SELECT id FROM widgets WHERE dashboard_id = $1",
        [dash.id],
      );
      expect(rows.length).toBe(1);
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });

  await ctx.close();
});

test("resize/move widget persists layout (POST /api/widgets/:id)", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQueryAndViz(api, async ({ queryId, vizId }) => {
    await executeAndWait(api, queryId);
    const dash = await api.createDashboard(`e2e-layout-${Date.now()}`);
    try {
      const widget = await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: vizId,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });

      // Position is server-owned and only the layout save writes it (drag-and-drop is too flaky).
      await api.updateWidget(widget.id, { width: 2 });
      await api.saveDashboardLayout(dash.slug, [
        { widget_id: widget.id, position: { col: 2, row: 1, sizeX: 4, sizeY: 3 } },
      ]);

      const [row] = await queryRows<{ width: number; options: { position?: Record<string, number> } }>(
        "SELECT width, options FROM widgets WHERE id = $1",
        [widget.id],
      );
      expect(row.width).toBe(2);
      expect(row.options.position).toMatchObject({ col: 2, row: 1, sizeX: 4, sizeY: 3 });

      // View mode shows the published snapshot; publish first.
      await api.publishDashboard(dash.slug);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });

  await ctx.close();
});

test("remove widget → disappears from dashboard", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withQueryAndViz(api, async ({ queryId, vizId }) => {
    await executeAndWait(api, queryId);
    const dash = await api.createDashboard(`e2e-remove-${Date.now()}`);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: vizId,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      // View mode shows the published snapshot by default.
      await api.publishDashboard(dash.slug);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });

      await page.click('button:has-text("Edit")');
      await page.locator("button.text-destructive").first().click();
      await page.locator("[role=dialog]").getByRole("button", { name: "Remove", exact: true }).click();

      await expect(page.getByText("Add your first widget")).toBeVisible({ timeout: 10_000 });
      await expect(page.locator("td").filter({ hasText: /^42$/ })).not.toBeVisible();

      const rows = await queryRows<{ id: string }>(
        "SELECT id FROM widgets WHERE dashboard_id = $1",
        [dash.id],
      );
      expect(rows.length).toBe(0);
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });

  await ctx.close();
});

test("non-admin cannot create a widget (404 — object-bound authz denials are uniform 404s, not 403)", async () => {
  const api = await loginAsAdmin();

  await withQueryAndViz(api, async ({ vizId }) => {
    const dash = await api.createDashboard(`e2e-authz-${Date.now()}`);
    try {
      const stamp = Date.now();
      const email = `e2e-viewer-${stamp}@example.com`;
      const password = "ViewerPass123!";
      const invited = await api.inviteUser({ name: "E2E Viewer", email });
      const [inv] = await queryRows<{ invitation_token: string }>(
        "SELECT details->>'invitation_token' AS invitation_token FROM users WHERE id = $1",
        [invited.id],
      );
      const viewer = new E2EApiClient();
      await viewer.acceptInvitation(inv.invitation_token, "E2E Viewer", password);
      await viewer.login(email, password);

      // authorize() denies with a uniform 404 so existence isn't leaked.
      await expect(
        viewer.createWidget({
          dashboard_id: dash.id,
          visualization_id: vizId,
          text: null,
          options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
        }),
      ).rejects.toThrow(/404/);

      await api.deleteUser(invited.id).catch(() => {});
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });
});

test("a viewer whose group can't reach the widget's data source sees the access boundary, not the never-run copy", async ({
  browser,
}) => {
  const api = await loginAsAdmin();

  await withQueryAndViz(api, async ({ vizId }) => {
    const dash = await api.createDashboard(`e2e-ds-boundary-${Date.now()}`);
    // view_* reach the tile; restricting data-source access with no allowlist entry must make its
    // data unreadable.
    const user = await seedCustomPermissionUser(api, "widget-ds-boundary", [
      "view_query",
      "view_dashboard",
    ]);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: vizId,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      await api.publishDashboard(dash.slug);
      await api.setGroupDataSourceRestricted(user.groupId, true);

      const ctx = await seededUserContext(browser, user);
      const page = await ctx.newPage();
      try {
        await page.goto(`/dashboard/${dash.slug}`);
        await expect(
          page.getByText("You don't have access to this widget's data source."),
        ).toBeVisible({ timeout: 10_000 });
        await expect(page.getByText("No data yet — this query hasn't run.")).toHaveCount(0);
      } finally {
        await ctx.close();
      }
    } finally {
      await user.cleanup();
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });
});

test("public dashboard with a widget renders cached query_result_data anonymously", async ({
  browser,
}) => {
  const api = await loginAsAdmin();

  await withQueryAndViz(api, async ({ queryId, vizId }) => {
    // A cached result must exist before sharing; the public endpoint embeds the latest cached
    // result.
    await executeAndWait(api, queryId);

    const dash = await api.createDashboard(`e2e-public-widget-${Date.now()}`);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: vizId,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      await api.publishDashboard(dash.slug);

      const { public_url } = await api.shareDashboard(dash.slug);
      expect(public_url).toContain("/public/dashboards/");

      const incognito = await browser.newContext();
      const publicPage = await incognito.newPage();
      await publicPage.goto(public_url);

      await expect(publicPage.getByText(dash.name)).toBeVisible({ timeout: 8000 });
      await expect(publicPage.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      // Unscheduled query is a snapshot; no age shown.
      await expect(publicPage.getByText(/Updated/)).toHaveCount(0);

      await incognito.close();
    } finally {
      await api.revokeShareDashboard(dash.slug).catch(() => {});
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  });
});

test.describe("tile freshness", () => {
  test("scheduled query feeds a fresh tile; a real broken refresh reddens it without blanking the number, on both private and public dashboards", async ({
    browser,
  }) => {
    test.setTimeout(90_000);
    const api = await loginAsAdmin();
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    const suffix = Date.now();

    const ds = await api.createDataSource({
      name: `e2e-freshness-ds-${suffix}`,
      type: "pg",
      options: TARGET_DS_OPTIONS,
    });
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `e2e-freshness-q-${suffix}`,
      query: "SELECT 42 AS val",
    });
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `e2e-freshness-viz-${suffix}`,
      options: {},
    });
    const dash = await api.createDashboard(`e2e-freshness-${suffix}`);

    try {
      await api.publishQuery(q.id);
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      await api.publishDashboard(dash.slug);

      await api.rawFetch("/api/schedules", {
        method: "POST",
        body: JSON.stringify({
          entity_type: "query",
          entity_id: q.id,
          spec: { kind: "interval", every_secs: 900 },
          enabled: true,
        }),
      });
      await api.rawFetch(`/api/queries/${q.id}/run`, { method: "POST" });
      await waitForRunRow(q.id, "success", 30_000);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      const freshDot = page.locator('span[title^="Updated "]');
      await expect(freshDot).toBeVisible();
      await expect(page.getByText("Data may be outdated")).not.toBeVisible();

      // Public dashboard shows the age inline (no hover on a TV).
      const { public_url } = await api.shareDashboard(dash.slug);
      const incognito = await browser.newContext();
      const publicPage = await incognito.newPage();
      await publicPage.goto(public_url);
      await expect(publicPage.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(publicPage.getByText(/Updated (just now|.* ago)/)).toBeVisible();
      await expect(publicPage.getByText("Data may be outdated")).not.toBeVisible();
      await incognito.close();

      await updateQuerySql(api, q.id, "SELECT * FROM nonexistent_table_zzz");
      await api.publishQuery(q.id);
      await api.rawFetch(`/api/queries/${q.id}/run`, { method: "POST" });
      await waitForRunRow(q.id, "failed", 30_000);

      await page.reload();
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(page.getByText("Data may be outdated")).toBeVisible();

      const incognito2 = await browser.newContext();
      const publicPage2 = await incognito2.newPage();
      await publicPage2.goto(public_url);
      await expect(publicPage2.locator("td").filter({ hasText: /^42$/ })).toBeVisible({
        timeout: 15_000,
      });
      await expect(publicPage2.getByText("Data may be outdated")).toBeVisible();
      await incognito2.close();
    } finally {
      await deleteRows("runs", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(
        () => {},
      );
      await deleteRows("schedules", "entity_type = 'query' AND entity_id = $1", [q.id]).catch(
        () => {},
      );
      await api.revokeShareDashboard(dash.slug).catch(() => {});
      await api.deleteDashboard(dash.slug).catch(() => {});
      await api.deleteVisualization(viz.id).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
      await api.deleteDataSource(ds.id).catch(() => {});
    }

    await ctx.close();
  });
});

test("Add Widget dialog resets after a successful add, so a second widget isn't a duplicate of the first", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();

  const ds = await api.createDataSource({
    name: `e2e-widget-reset-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  let queryAId: number | undefined;
  let queryBId: number | undefined;
  let vizAId: number | undefined;
  let vizBId: number | undefined;
  const queryAName = `e2e-widget-reset-a-${stamp}`;
  const queryBName = `e2e-widget-reset-b-${stamp}`;

  try {
    const qA = await api.createQuery({
      data_source_id: ds.id,
      name: queryAName,
      query: "SELECT 42 AS val",
    });
    queryAId = qA.id;
    await api.publishQuery(qA.id);
    const vizA = await api.createVisualization({
      query_id: qA.id,
      type: "table",
      name: `e2e-widget-reset-viz-a-${stamp}`,
      options: {},
    });
    vizAId = vizA.id;
    await executeAndWait(api, qA.id);

    const qB = await api.createQuery({
      data_source_id: ds.id,
      name: queryBName,
      query: "SELECT 7 AS val",
    });
    queryBId = qB.id;
    await api.publishQuery(qB.id);
    const vizB = await api.createVisualization({
      query_id: qB.id,
      type: "table",
      name: `e2e-widget-reset-viz-b-${stamp}`,
      options: {},
    });
    vizBId = vizB.id;
    await executeAndWait(api, qB.id);

    const dash = await api.createDashboard(`e2e-widget-reset-${stamp}`);
    try {
      await page.goto(`/dashboard/${dash.slug}`);
      await page.click('button:has-text("Edit")');

      await page.click('button:has-text("Add Widget")');
      let dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      let querySelect = page.getByTestId("add-widget-query-select");
      await querySelect.click();
      await page.getByRole("option", { name: queryAName }).click();
      let vizSelect = page.getByTestId("add-widget-viz-select");
      await expect(vizSelect).toBeVisible({ timeout: 8000 });
      await vizSelect.click();
      await page.getByRole("option").first().click();
      await page.locator('[role=dialog] button:has-text("Add")').click();
      await expect(dialog).not.toBeVisible({ timeout: 8000 });
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });

      await page.click('button:has-text("Add Widget")');
      dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      querySelect = page.getByTestId("add-widget-query-select");
      await expect(querySelect).toContainText("Select a query…");
      await expect(page.getByTestId("add-widget-viz-select")).toHaveCount(0);
      await expect(page.getByTestId("add-widget-query-filter")).toHaveValue("");

      await querySelect.click();
      await page.getByRole("option", { name: queryBName }).click();
      vizSelect = page.getByTestId("add-widget-viz-select");
      await expect(vizSelect).toBeVisible({ timeout: 8000 });
      await vizSelect.click();
      await page.getByRole("option").first().click();
      await page.locator('[role=dialog] button:has-text("Add")').click();
      await expect(dialog).not.toBeVisible({ timeout: 8000 });

      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("td").filter({ hasText: /^7$/ })).toBeVisible({ timeout: 15_000 });
      const rows = await queryRows<{ id: string }>(
        "SELECT id FROM widgets WHERE dashboard_id = $1",
        [dash.id],
      );
      expect(rows.length).toBe(2);
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  } finally {
    if (vizAId) await api.deleteVisualization(vizAId).catch(() => {});
    if (vizBId) await api.deleteVisualization(vizBId).catch(() => {});
    if (queryAId) await api.deleteQuery(queryAId).catch(() => {});
    if (queryBId) await api.deleteQuery(queryBId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("counter with 2 decimal places renders 4,222.90 on the dashboard tile", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();
  const ds = await api.createDataSource({ name: `e2e-counter-ds-${stamp}`, type: "pg", options: TARGET_DS_OPTIONS });
  let queryId: number | undefined;
  let vizId: number | undefined;
  try {
    const q = await api.createQuery({ data_source_id: ds.id, name: `e2e-counter-q-${stamp}`, query: "select 4222.9 as v" });
    queryId = q.id;
    await api.publishQuery(q.id);
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "COUNTER",
      name: `e2e-counter-viz-${stamp}`,
      options: { columnName: "v", numberDecimals: 2, numberGrouping: true },
    });
    vizId = viz.id;
    await executeAndWait(api, q.id);

    const dash = await api.createDashboard(`e2e-counter-fmt-${stamp}`);
    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.getByText("4,222.90", { exact: true })).toBeVisible({ timeout: 15_000 });
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
    }
  } finally {
    if (vizId) await api.deleteVisualization(vizId).catch(() => {});
    if (queryId) await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("dragging a tile by its header saves exactly one layout PUT; entering edit mode saves none", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const dash = await api.createDashboard(`e2e-drag-${Date.now()}`);
  try {
    const mk = (text: string, col: number) =>
      api.createWidget({
        dashboard_id: dash.id,
        visualization_id: null,
        text,
        options: { position: { col, row: 0, sizeX: 3, sizeY: 2 } },
      });
    const a = await mk("tile a", 0);
    await mk("tile b", 3);
    await api.publishDashboard(dash.slug);

    const layoutPuts: string[] = [];
    page.on("request", (req) => {
      if (req.method() === "PUT" && req.url().endsWith(`/api/dashboards/${dash.slug}/layout`)) {
        layoutPuts.push(req.url());
      }
    });

    await page.goto(`/dashboard/${dash.slug}`);
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    const handles = page.locator(".tile-drag-handle");
    await expect(handles).toHaveCount(2);

    await page.waitForTimeout(1000);
    expect(layoutPuts).toHaveLength(0);

    const grid = page.locator(".react-grid-layout");
    const topBefore = (await grid.boundingBox())!.y;
    const from = (await handles.nth(0).boundingBox())!;
    const to = (await handles.nth(1).boundingBox())!;

    await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
    await page.mouse.down();
    await page.mouse.move(from.x + from.width / 2 + 20, from.y + from.height / 2 + 5, { steps: 5 });
    await page.mouse.move(to.x + to.width / 2, to.y + to.height / 2 + 10, { steps: 15 });
    await page.mouse.up();

    await expect.poll(() => layoutPuts.length).toBe(1);
    await expect
      .poll(async () => {
        const [row] = await queryRows<{ options: { position?: { col: number; row: number } } }>(
          "SELECT options FROM widgets WHERE id = $1",
          [a.id],
        );
        return row.options.position?.col !== 0 || row.options.position?.row !== 0;
      })
      .toBe(true);

    // The always-present banner means the save does not shift the grid.
    await expect.poll(async () => (await grid.boundingBox())!.y).toBe(topBefore);
    expect(layoutPuts).toHaveLength(1);
  } finally {
    await api.deleteDashboard(dash.slug).catch(() => {});
  }
  await ctx.close();
});
