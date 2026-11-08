// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext, loginAsAnyLanding } from "./helpers/auth";
import { E2EApiClient, executeAndWait, loginAsAdmin, pollJob, withTargetDs } from "./helpers/api";
import { getQuery, queryRows } from "./helpers/db";
import { clickActionBarItem } from "./helpers/actions";
import { typeSql } from "./helpers/editor";
import { invitationToken } from "./helpers/permissions";

async function getExecutionState(queryId: number) {
  const rows = await queryRows<{
    last_execution_status: string;
    last_execution_at: string | null;
  }>("SELECT last_execution_status, last_execution_at FROM queries WHERE id = $1", [queryId]);
  return rows[0];
}



test("create query → appears in query list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `list-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto("/queries");
      await expect(page.getByText(q.name)).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("execute SELECT 1 → results table shows '1'", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `exec-test-${Date.now()}`,
      query: "SELECT 1 AS val",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await page.click('button:has-text("Execute")');

      await expect(page.locator("td").filter({ hasText: /^1$/ })).toBeVisible({
        timeout: 20_000,
      });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("save query → name appears in list (UI + DB)", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `save-ui-${Date.now()}`;

    try {
      await page.goto("/queries/new");

      await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });
      await page.fill("#query-name", name);
      // A name alone doesn't create the row; real SQL (not the "SELECT 1" scaffold) does.
      await typeSql(page, "SELECT 1 AS val");

      // Autosave-only: POSTs the query and swaps the URL to the new id once a name and real SQL
      // exist.
      await page.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });

      const row = await getQuery(name);
      expect(row).not.toBeNull();
      expect(row!.published).toBe(false);
    } finally {
      // Look up by name: a timeout after autosave fired but before the id was captured would strand
      // the row.
      const row = await getQuery(name);
      if (row) await api.deleteQuery(Number(row.id)).catch(() => {});
    }
  });

  await ctx.close();
});

test("new-query screen explains it hasn't saved yet, then names a name-only draft never persisted", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `unsaved-name-only-${Date.now()}`;

  await page.goto("/queries/new");
  await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });

  await expect(
    page.getByText("Add SQL and a name — or run it — to save this query."),
  ).toBeVisible({ timeout: 8000 });

  await page.fill("#query-name", name);
  // A name alone must not create the row; wait past autosave's ~1s debounce, then assert no URL
  // change or DB row.
  await page.waitForTimeout(2000);

  expect(page.url()).toMatch(/\/queries\/new$/);
  const row = await getQuery(name);
  expect(row).toBeNull();

  await ctx.close();
});

test("new-query screen never saves a draft from real SQL alone — untouched name blocks it", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/queries/new");
  await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });

  await typeSql(page, "SELECT * FROM widgets");
  await page.waitForTimeout(2000);

  expect(page.url()).toMatch(/\/queries\/new$/);
  await expect(
    page.getByText("Add SQL and a name — or run it — to save this query."),
  ).toBeVisible();

  await ctx.close();
});

test("executing a brand-new query with untouched scaffold SQL creates the row", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    let queryId: number | null = null;
    try {
      await page.goto("/queries/new");
      await page.waitForSelector(`[id="query-name"]`, { timeout: 8000 });

      const nameInput = page.locator("#query-name");
      const draftName = await nameInput.inputValue();
      expect(draftName.length).toBeGreaterThan(0);

      await page.click('button:has-text("Execute")');

      // Running is itself worth saving; the row is created with the run.
      await page.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
      const row = await getQuery(draftName);
      expect(row).not.toBeNull();
      queryId = Number(row!.id);
      expect(row!.query).toBe("SELECT 1");
    } finally {
      if (queryId != null) await api.deleteQuery(queryId).catch(() => {});
    }
  });

  await ctx.close();
});

test("Snippets doesn't overlap the editor tab strip at a 390px mobile width", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await page.setViewportSize({ width: 390, height: 844 });

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `mobile-tabs-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      const sqlTab = page.getByRole("button", { name: "SQL Editor" });
      const snippetsBtn = page.getByRole("button", { name: "Snippets" });
      await expect(sqlTab).toBeVisible({ timeout: 8000 });
      await expect(snippetsBtn).toBeVisible();

      const tabBox = await sqlTab.boundingBox();
      const snippetsBox = await snippetsBtn.boundingBox();
      expect(tabBox).not.toBeNull();
      expect(snippetsBox).not.toBeNull();

      const tabBottom = tabBox!.y + tabBox!.height;
      expect(snippetsBox!.y).toBeGreaterThanOrEqual(tabBottom);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("archive query → disappears from default list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `archive-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      // Archive lives in the "More actions" overflow menu at this viewport.
      await clickActionBarItem(page, "Archive");
      await expect(page.getByText("Archived")).toBeVisible({ timeout: 8000 });

      await page.goto("/queries");
      await expect(page.getByText(q.name)).not.toBeVisible();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("DB assertion: create query row exists in DB", async () => {
  const api = await loginAsAdmin();
  await withTargetDs(api, async (dsId) => {
    const name = `db-check-${Date.now()}`;
    const q = await api.createQuery({
      data_source_id: dsId,
      name,
      query: "SELECT 42",
    });
    try {
      const row = await getQuery(name);
      expect(row).not.toBeNull();
      expect(row!.published).toBe(false);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });
});

test("publish: confirm dialog flips published to true in DB", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `publish-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      // Publish lives in the overflow menu at this viewport and opens a confirm dialog.
      await clickActionBarItem(page, "Publish");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await dialog.getByRole("button", { name: "Publish", exact: true }).click();
      await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

      const row = await getQuery(q.name);
      expect(row!.published).toBe(true);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("unpublish returns a query to draft, and it can be published again", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `unpublish-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await api.publishQuery(q.id);
      await page.goto(`/queries/${q.id}`);
      await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

      await clickActionBarItem(page, "Unpublish");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await expect(dialog.getByText(/workspace loses access/i)).toBeVisible();
      await expect(dialog.getByText(/public links and embeds stop working/i)).toBeVisible();
      await dialog.getByRole("button", { name: "Unpublish", exact: true }).click();
      await expect(dialog).toBeHidden({ timeout: 8000 });

      await expect(page.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });
      const row = await getQuery(q.name);
      expect(row!.published).toBe(false);

      await clickActionBarItem(page, "Publish");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await dialog.getByRole("button", { name: "Publish", exact: true }).click();
      await expect(page.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });
      const republished = await getQuery(q.name);
      expect(republished!.published).toBe(true);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("query with parameter → parameter input appears", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `param-test-${Date.now()}`,
      query: "SELECT {{ num }} AS result",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await expect(page.getByTestId("param-num")).toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("draft query: Refresh schedule shows 'Publish to enable' and its controls stay locked", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `draft-schedule-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      const toggle = page.getByRole("button", { name: "Refresh schedule" });
      await expect(toggle).toBeVisible({ timeout: 8000 });
      await expect(toggle.getByText("Publish to enable")).toBeVisible();
      await toggle.click();
      await expect(page.getByText("Publish this query to schedule refreshes.")).toBeVisible();
      await expect(page.getByRole("switch")).toBeDisabled();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("draft query: creating a schedule is rejected until the query is published", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `draft-scheduled-badge-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await expect(
        api.rawFetch("/api/schedules", {
          method: "POST",
          body: JSON.stringify({
            entity_type: "query",
            entity_id: q.id,
            spec: { kind: "interval", every_secs: 3600 },
          }),
        }),
      ).rejects.toThrow(/publish this query/i);

      await page.goto(`/queries/${q.id}`);
      await expect(page.getByRole("button", { name: "Refresh schedule" })).toContainText(
        "Publish to enable",
        { timeout: 8000 },
      );
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("published query: QueriesPage row shows the active schedule cadence", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `published-scheduled-list-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await api.publishQuery(q.id);
      await api.rawFetch("/api/schedules", {
        method: "POST",
        body: JSON.stringify({
          entity_type: "query",
          entity_id: q.id,
          spec: { kind: "interval", every_secs: 3600 },
        }),
      });

      // The Schedule column needs a container wider than 64rem; widen the viewport.
      await page.setViewportSize({ width: 1440, height: 900 });
      await page.goto("/queries");
      // TableRow has role="button", so getByRole("row") never matches; use the <tr> tag.
      const row = page.locator("tr", { hasText: q.name });
      await expect(row).toBeVisible({ timeout: 8000 });
      await expect(row.getByText("Hourly")).toBeVisible();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("Owner column shows the query creator's name", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    // OwnerChip shows "You" for the viewer's own rows, so create the query as a second user.
    const groups = await api.listGroups();
    const adminGroup = groups.find((g) => g.name === "admin");
    expect(adminGroup).toBeDefined();

    const ownerName = `E2E Owner ${Date.now()}`;
    const email = `e2e-owner-col-${Date.now()}@example.com`;
    const invited = await api.inviteUser({ name: ownerName, email, groups: [adminGroup!.id] });
    const token = await invitationToken(invited.id);
    const userApi = new E2EApiClient();
    await userApi.acceptInvitation(token, ownerName, "OwnerColE2E123!");

    const q = await userApi.createQuery({
      data_source_id: dsId,
      name: `owner-col-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      // The Owner column needs a container wider than 71rem; widen the viewport.
      await page.setViewportSize({ width: 1600, height: 900 });
      await page.goto("/queries");
      const row = page.locator("tr", { hasText: q.name });
      await expect(row).toBeVisible({ timeout: 8000 });
      await expect(row.getByText(ownerName)).toBeVisible();
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("Draft/Published status filter shows only matching queries", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const draftQ = await api.createQuery({
      data_source_id: dsId,
      name: `filter-draft-${Date.now()}`,
      query: "SELECT 1",
    });
    const publishedQ = await api.createQuery({
      data_source_id: dsId,
      name: `filter-published-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      const full = await api.rawFetch<{ working_version: number }>(
        `/api/queries/${publishedQ.id}`,
      );
      await api.rawFetch(`/api/queries/${publishedQ.id}/publish`, {
        method: "POST",
        body: JSON.stringify({ expected_version: full.working_version }),
      });

      await page.goto("/queries");

      await page.getByRole("button", { name: "Draft", exact: true }).click();
      await expect(page.locator("tr", { hasText: draftQ.name })).toBeVisible({ timeout: 8000 });
      await expect(page.locator("tr", { hasText: publishedQ.name })).not.toBeVisible();

      await page.getByRole("button", { name: "Published", exact: true }).click();
      await expect(page.locator("tr", { hasText: publishedQ.name })).toBeVisible({ timeout: 8000 });
      await expect(page.locator("tr", { hasText: draftQ.name })).not.toBeVisible();
    } finally {
      await api.deleteQuery(draftQ.id).catch(() => {});
      await api.deleteQuery(publishedQ.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("'By me' scope filter shows only the current user's own queries", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    // Reuse the builtin "admin" group for data-source access + create_query; only q.user_id matters
    // here.
    const groups = await api.listGroups();
    const adminGroup = groups.find((g) => g.name === "admin");
    expect(adminGroup).toBeDefined();

    const email = `e2e-scope-${Date.now()}@example.com`;
    const invited = await api.inviteUser({ name: "E2E Scope", email, groups: [adminGroup!.id] });
    const token = await invitationToken(invited.id);
    const userApi = new E2EApiClient();
    await userApi.acceptInvitation(token, "E2E Scope", "ScopePass123!");
    await userApi.login(email, "ScopePass123!");

    const adminQuery = await api.createQuery({
      data_source_id: dsId,
      name: `scope-admin-${Date.now()}`,
      query: "SELECT 1",
    });
    const otherQuery = await userApi.createQuery({
      data_source_id: dsId,
      name: `scope-other-${Date.now()}`,
      query: "SELECT 1",
    });

    try {
      await page.goto("/queries");

      await page.getByRole("button", { name: "By me", exact: true }).click();
      await expect(page.locator("tr", { hasText: adminQuery.name })).toBeVisible({ timeout: 8000 });
      await expect(page.locator("tr", { hasText: otherQuery.name })).not.toBeVisible();

      await page.getByRole("button", { name: "All owners", exact: true }).click();
      await expect(page.locator("tr", { hasText: adminQuery.name })).toBeVisible();
      await expect(page.locator("tr", { hasText: otherQuery.name })).toBeVisible();
    } finally {
      // Admin deletes both: unconditional access regardless of owner.
      await api.deleteQuery(adminQuery.id).catch(() => {});
      await api.deleteQuery(otherQuery.id).catch(() => {});
      await api.deleteUser(invited.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("toolbar disambiguates its two filter groups as 'All owners' and 'All states'", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await page.goto("/queries");
  await expect(page.getByRole("button", { name: "All owners", exact: true })).toBeVisible({
    timeout: 8000,
  });
  await expect(page.getByRole("button", { name: "All states", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "All", exact: true })).toHaveCount(0);

  await ctx.close();
});


test("delete dialog shows the impact line; confirming removes the monitor and dashboard tile without a manual reload", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const stamp = Date.now();
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `delete-impact-${stamp}`,
      query: "SELECT 1 AS val",
    });
    await api.publishQuery(q.id);
    const vizName = `delete-impact-viz-${stamp}`;
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: vizName,
      options: {},
    });
    const dash = await api.createDashboard(`delete-impact-dash-${stamp}`);
    await api.createWidget({
      dashboard_id: dash.id,
      visualization_id: viz.id,
      text: null,
      options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
    });
    await api.publishDashboard(dash.slug);
    const monitor = await api.createMonitor({
      query_id: q.id,
      name: `delete-impact-mon-${stamp}`,
      options: { column: "val", op: ">", value: 0 },
    });

    try {
      await page.goto("/queries");
      const row = page.locator("tr", { hasText: q.name });
      await expect(row).toBeVisible({ timeout: 8000 });
      await row.getByRole("button", { name: "Delete" }).click();

      const dialog = page.getByRole("dialog");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await expect(
        dialog.getByText("Feeds 1 tile on 1 dashboard · 1 monitor. Deleting this query removes them."),
      ).toBeVisible({ timeout: 8000 });

      await dialog.getByRole("button", { name: "Delete", exact: true }).click();
      await expect(dialog).not.toBeVisible({ timeout: 8000 });
      await expect(row).not.toBeVisible();

      // In-app navigation proves the ["monitors"] cache invalidation refetches.
      await page.getByRole("link", { name: "Monitors", exact: true }).click();
      await expect(page.getByText(monitor.name)).not.toBeVisible({ timeout: 8000 });

      // Same for the dashboard tile, which is pruned at read time.
      await page.getByRole("link", { name: "Dashboards", exact: true }).click();
      await page.getByRole("link", { name: dash.name }).click();
      await expect(page.getByText(vizName)).not.toBeVisible({ timeout: 8000 });
    } finally {
      await api.deleteMonitor(monitor.id).catch(() => {});
      await api.deleteDashboard(dash.slug).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("editor Execute on an unedited saved query leaves the queries list at Success, not 'Never'; an unsaved SQL edit leaves that state untouched", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `exec-state-bind-${Date.now()}`,
      query: "SELECT 1 AS val",
    });
    try {
      await page.goto(`/queries/${q.id}`);

      let state = await getExecutionState(q.id);
      expect(state.last_execution_status).toBe("New");
      expect(state.last_execution_at).toBeNull();

      // Execute with no unsaved edits must bind execution state like any other path.
      await page.click('button:has-text("Execute")');
      await expect(page.locator("td").filter({ hasText: /^1$/ })).toBeVisible({ timeout: 20_000 });

      state = await getExecutionState(q.id);
      expect(state.last_execution_status).toBe("Success");
      expect(state.last_execution_at).not.toBeNull();
      const boundAt = new Date(state.last_execution_at!).getTime();

      await page.goto("/queries");
      const row = page.locator("tr", { hasText: q.name });
      await expect(row).toBeVisible({ timeout: 8000 });
      await expect(row.getByText("Success")).toBeVisible();
      await expect(row.getByText("Never")).not.toBeVisible();

      // An unsaved, diverging run must not touch the saved query's execution state.
      await page.goto(`/queries/${q.id}`);
      await typeSql(page, "SELECT 2 AS val");
      await page.click('button:has-text("Execute")');
      await expect(page.locator("td").filter({ hasText: /^2$/ })).toBeVisible({ timeout: 20_000 });

      state = await getExecutionState(q.id);
      expect(state.last_execution_status).toBe("Success");
      expect(new Date(state.last_execution_at!).getTime()).toBe(boundAt);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("editor Execute always forces a fresh run: two consecutive Executes both enqueue a job, never a cache replay", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `exec-fresh-run-${Date.now()}`,
      query: "SELECT 1 AS val",
    });
    try {
      await page.goto(`/queries/${q.id}`);

      for (let i = 0; i < 2; i++) {
        const [response] = await Promise.all([
          page.waitForResponse(
            (res) => res.url().includes("/api/query_results") && res.request().method() === "POST",
          ),
          page.click('button:has-text("Execute")'),
        ]);
        const body = await response.json();
        expect(body.job, `execute #${i + 1} response: ${JSON.stringify(body)}`).toBeTruthy();
        expect(body.query_result, `execute #${i + 1} must not be a cache replay`).toBeUndefined();
        await expect(page.locator("td").filter({ hasText: /^1$/ })).toBeVisible({ timeout: 20_000 });
      }
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("opening a saved query with an existing stored result shows it immediately, labeled with its age, with no Execute click", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `stored-result-on-open-${Date.now()}`,
      query: "SELECT 7 AS val",
    });
    try {
      // Seed via the API: opening the editor must not re-run the query.
      await executeAndWait(api, q.id);

      let executePostFired = false;
      page.on("request", (req) => {
        if (req.method() === "POST" && req.url().includes("/api/query_results")) {
          executePostFired = true;
        }
      });

      await page.goto(`/queries/${q.id}`);

      await expect(page.locator("td").filter({ hasText: /^7$/ })).toBeVisible({ timeout: 8000 });
      // timeAgo() may render "just now" or "N ago".
      await expect(page.getByText(/Updated (just now|.*ago)/i)).toBeVisible();
      await expect(page.getByText("Run the query to see results.")).not.toBeVisible();
      expect(executePostFired, "opening the query must not itself trigger an execution").toBe(
        false,
      );
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});

test("a query that has never been executed shows the empty-results prompt, not an error, on open", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `never-run-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await expect(page.getByText("Run the query to see results.")).toBeVisible({
        timeout: 8000,
      });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("queries list: the row's name is a real link, and clicking elsewhere in the row navigates while an action button does not", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `row-semantics-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto("/queries");
      const row = page.locator("tr", { hasText: q.name });
      await expect(row).toBeVisible({ timeout: 8000 });

      const nameLink = row.getByRole("link", { name: q.name });
      await expect(nameLink).toHaveAttribute("href", `/queries/${q.id}`);

      // The delete dialog's click must not also navigate the row.
      await row.getByRole("button", { name: "Delete" }).click();
      await expect(page.getByRole("dialog")).toBeVisible();
      await expect(page).toHaveURL(/\/queries$/);
      await page.getByRole("button", { name: "Cancel" }).click();
      await expect(page.getByRole("dialog")).not.toBeVisible();

      await row.getByRole("cell").nth(1).click();
      await expect(page).toHaveURL(new RegExp(`/queries/${q.id}$`), { timeout: 8000 });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("forking a published query as a non-editor shows the published SQL, not a pending draft edit", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();

  await withTargetDs(adminApi, async (dsId) => {
    const publishedMarker = `e2e_fork_published_${stamp}`;
    const draftMarker = `e2e_fork_draft_only_${stamp}`;

    const q = await adminApi.createQuery({
      data_source_id: dsId,
      name: `e2e-fork-nonedit-${stamp}`,
      query: `SELECT '${publishedMarker}' AS marker`,
    });
    await adminApi.publishQuery(q.id);
    const full = await adminApi.rawFetch<{ working_version: number }>(`/api/queries/${q.id}`);
    await adminApi.updateQuery(q.id, {
      query: `SELECT '${draftMarker}' AS marker`,
      expected_version: full.working_version,
    });

    // Non-editor: view_query + create_query, only a Viewer ACL grant on this query.
    const role = await adminApi.createRole({
      name: `e2e-fork-role-${stamp}`,
      permissions: ["view_query", "create_query"],
    });
    const group = await adminApi.createGroup({ name: `e2e-fork-group-${stamp}` });
    await adminApi.addGroupRole(group.id, role.id);

    const email = `e2e-fork-bob-${stamp}@example.com`;
    const password = "ForkE2E123!";
    const invited = await adminApi.inviteUser({ name: "Fork Bob", email, groups: [group.id] });
    const rows = await queryRows<{ token: string | null }>(
      "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
      [invited.id],
    );
    const token = rows[0]?.token;
    if (!token) throw new Error(`invitation_token missing for user ${invited.id}`);
    const bobApi = new E2EApiClient();
    await bobApi.acceptInvitation(token, "Fork Bob", password);

    await adminApi.rawFetch(`/api/queries/${q.id}/acl`, {
      method: "POST",
      body: JSON.stringify({ user_id: invited.id, access_type: "view" }),
    });

    const bobCtx = await browser.newContext();
    const bobPage = await bobCtx.newPage();

    let forkedId: number | undefined;
    try {
      await loginAsAnyLanding(bobPage, email, password);
      await bobPage.goto(`/queries/${q.id}`);
      await expect(bobPage.locator(".cm-content")).toContainText(publishedMarker, { timeout: 8000 });

      await clickActionBarItem(bobPage, "Fork");
      await bobPage.waitForURL((url) => /\/queries\/\d+$/.test(url.pathname) && url.pathname !== `/queries/${q.id}`, {
        timeout: 10_000,
      });
      const match = /\/queries\/(\d+)$/.exec(new URL(bobPage.url()).pathname);
      forkedId = match ? Number(match[1]) : undefined;
      expect(forkedId).toBeDefined();

      await expect(bobPage.locator(".cm-content")).toContainText(publishedMarker, { timeout: 8000 });
      await expect(bobPage.locator(".cm-content")).not.toContainText(draftMarker);
    } finally {
      await bobCtx.close();
      if (forkedId) await adminApi.deleteQuery(forkedId).catch(() => {});
      await adminApi.deleteUser(invited.id).catch(() => {});
      await adminApi.deleteGroup(group.id).catch(() => {});
      await adminApi.deleteRole(role.id).catch(() => {});
      await adminApi.deleteQuery(q.id).catch(() => {});
    }
  });
});


// Runs a saved query via POST /api/queries/:id/results and waits for the cached result.
async function executeSavedAndWait(
  api: E2EApiClient,
  queryId: number,
  parameters: unknown,
): Promise<number> {
  const res = await api.rawFetch<{ query_result?: { id: number }; job?: { id: string } }>(
    `/api/queries/${queryId}/results`,
    { method: "POST", body: JSON.stringify({ parameters, max_age: 0 }) },
  );
  if (res.query_result) return res.query_result.id;
  if (res.job) return pollJob(api, res.job.id);
  throw new Error("execute_saved: unexpected response shape");
}

test("a dashboard viewer's filter re-run never moves the published pointer other viewers read", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const stamp = Date.now();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-pointer-${stamp}`,
      query: "SELECT '{{ region }}' AS region_val",
    });

    const full = await api.rawFetch<{ working_version: number }>(`/api/queries/${q.id}`);
    await api.rawFetch(`/api/queries/${q.id}`, {
      method: "POST",
      body: JSON.stringify({
        options: {
          parameters: [
            {
              name: "region",
              title: "Region",
              type: "dropdown",
              enum_options: "hello\nworld",
              value: "hello",
            },
          ],
        },
        expected_version: full.working_version,
      }),
    });
    await api.publishQuery(q.id);

    // The canonical run (no caller values) may move the published pointer.
    const canonicalResultId = await executeSavedAndWait(api, q.id, null);
    const publishedBefore = await api.rawFetch<{ query_result: { id: number } }>(
      `/api/queries/${q.id}/results?published_only=true`,
    );
    expect(publishedBefore.query_result.id).toBe(canonicalResultId);

    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `e2e-pointer-viz-${stamp}`,
      options: {},
    });
    const dash = await api.createDashboard(`e2e-pointer-dash-${stamp}`);

    try {
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      await api.publishDashboard(dash.slug);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^hello$/ })).toBeVisible({
        timeout: 15_000,
      });

      // Switching the dashboard filter drives WidgetCard's own re-run with the viewer's filter
      // values.
      await page.getByTestId("dashboard-param-region").getByRole("combobox").click();
      await page.getByRole("option", { name: "world" }).click();
      await expect(page.locator("td").filter({ hasText: /^world$/ })).toBeVisible({
        timeout: 15_000,
      });

      // The published view must still be the canonical run.
      const publishedAfter = await api.rawFetch<{ query_result: { id: number } }>(
        `/api/queries/${q.id}/results?published_only=true`,
      );
      expect(publishedAfter.query_result.id).toBe(canonicalResultId);
    } finally {
      await api.deleteDashboard(dash.slug).catch(() => {});
      await api.deleteVisualization(viz.id).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});
