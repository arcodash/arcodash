// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect, type Page } from "@playwright/test";
import { adminContext, loginAs } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin, executeAndWait, withTargetDs } from "./helpers/api";
import { getQuery, queryRows } from "./helpers/db";
import { clickActionBarItem } from "./helpers/actions";
import { typeSql } from "./helpers/editor";
import { invitationToken } from "./helpers/permissions";

const PASSWORD = "EditCommitE2E123!";

async function createOrgMember(adminApi: E2EApiClient, name: string, email: string): Promise<number> {
  const invited = await adminApi.inviteUser({ name, email, groups: [] });
  const token = await invitationToken(invited.id);
  await new E2EApiClient().acceptInvitation(token, name, PASSWORD);
  return invited.id;
}

// label is "Publish" for a never-published query, "Publish changes" once published.
async function publishViaUi(page: Page, label: "Publish" | "Publish changes") {
  await clickActionBarItem(page, label);
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.getByRole("button", { name: label, exact: true }).click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
}


test("edit a published query stays invisible to the workspace until publish", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const stamp = Date.now();
  const email = `ec-accept-b-${stamp}@example.com`;
  const bId = await createOrgMember(adminApi, "EC Accept B", email);
  // The viewer's group needs data-source access plus an org-level view permission (via the builtin
  // "Viewer" role, not a group column).
  const group = await adminApi.createGroup({ name: `ec-accept-group-${stamp}` });
  await adminApi.addGroupMember(group.id, bId);
  const roles = await adminApi.listRoles();
  const viewerRole = roles.find((r) => r.name === "Viewer" && r.type === "builtin");
  if (!viewerRole) throw new Error("builtin 'Viewer' role not found");
  await adminApi.addGroupRole(group.id, viewerRole.id);

  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const bCtx = await browser.newContext();
  const bPage = await bCtx.newPage();
  await loginAs(bPage, email, PASSWORD);

  try {
    await withTargetDs(adminApi, async (dsId) => {
      await adminApi.rawFetch(`/api/groups/${group.id}/data_sources`, {
        method: "POST",
        body: JSON.stringify({ data_source_id: dsId }),
      });

      const name = `ec-accept-query-${stamp}`;
      const q = await adminApi.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
      try {
        await adminPage.goto(`/queries/${q.id}`);
        await expect(adminPage.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });
        await publishViaUi(adminPage, "Publish");
        await expect(adminPage.getByText("Published", { exact: true })).toBeVisible({ timeout: 8000 });

        await bPage.goto(`/queries/${q.id}`);
        await expect(bPage.locator("#query-name")).toHaveValue(name, { timeout: 8000 });

        const editedName = `${name}-edited`;
        await adminPage.fill("#query-name", editedName);
        await expect(adminPage.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });
        await expect(adminPage.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

        await bPage.reload();
        await expect(bPage.locator("#query-name")).toHaveValue(name, { timeout: 8000 });

        await publishViaUi(adminPage, "Publish changes");

        await bPage.reload();
        await expect(bPage.locator("#query-name")).toHaveValue(editedName, { timeout: 8000 });
      } finally {
        await adminApi.deleteQuery(q.id).catch(() => {});
      }
    });
  } finally {
    await adminApi.deleteUser(bId).catch(() => {});
    await adminApi.deleteGroup(group.id).catch(() => {});
    await adminCtx.close();
    await bCtx.close();
  }
});


test("discard changes restores the draft to the published version", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-discard-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1 AS val" });
    try {
      await page.goto(`/queries/${q.id}`);
      await publishViaUi(page, "Publish");

      await typeSql(page, "SELECT 2 AS val");
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });
      await expect(page.locator(".cm-content")).toContainText("SELECT 2");

      await clickActionBarItem(page, "Discard changes");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await dialog.getByRole("button", { name: "Discard changes", exact: true }).click();
      await expect(dialog).not.toBeVisible({ timeout: 8000 });

      await expect(page.locator(".cm-content")).toContainText("SELECT 1", { timeout: 8000 });
      await expect(page.getByText("Unpublished changes")).toHaveCount(0);

      const [row] = await queryRows<{ query: string; published_query: string }>(
        "SELECT query, published_query FROM queries WHERE id = $1",
        [q.id],
      );
      expect(row.query.trim()).toBe(row.published_query.trim());
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("autosave persists across reload with no Save button, and the workspace stays on the published version", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-autosave-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
    try {
      await page.goto(`/queries/${q.id}`);
      await publishViaUi(page, "Publish");

      await expect(page.getByRole("button", { name: "Save", exact: true })).toHaveCount(0);

      const editedName = `${name}-autosaved`;
      await page.fill("#query-name", editedName);
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(page.getByRole("button", { name: "Save", exact: true })).toHaveCount(0);

      await page.reload();
      await expect(page.locator("#query-name")).toHaveValue(editedName, { timeout: 8000 });
      await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });
      await expect(page.getByRole("button", { name: "Save", exact: true })).toHaveCount(0);

      // Check the DB: the editor is the only surface allowed to see the draft.
      const [row] = await queryRows<{ name: string; published_name: string }>(
        "SELECT name, published_name FROM queries WHERE id = $1",
        [q.id],
      );
      expect(row.name).toBe(editedName);
      expect(row.published_name).toBe(name);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("per-version execute+results: editor draft preview differs from the published dashboard tile", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const stamp = Date.now();
    const name = `ec-perversion-${stamp}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 42 AS val" });
    const viz = await api.createVisualization({
      query_id: q.id,
      type: "table",
      name: `ec-perversion-viz-${stamp}`,
      options: {},
    });
    let dashSlug = "";
    try {
      await executeAndWait(api, q.id);

      await page.goto(`/queries/${q.id}`);
      await publishViaUi(page, "Publish");

      const dash = await api.createDashboard(`ec-perversion-dash-${stamp}`);
      dashSlug = dash.slug;
      await api.createWidget({
        dashboard_id: dash.id,
        visualization_id: viz.id,
        text: null,
        options: { position: { col: 0, row: 0, sizeX: 3, sizeY: 2 } },
      });
      // View mode shows the published snapshot; publish so the tile is visible.
      await api.publishDashboard(dash.slug);

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });

      await page.goto(`/queries/${q.id}`);
      await typeSql(page, "SELECT 99 AS val");
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });

      await page.click('button:has-text("Execute")');
      await expect(page.locator("td").filter({ hasText: /^99$/ })).toBeVisible({ timeout: 20_000 });

      await page.goto(`/dashboard/${dash.slug}`);
      await expect(page.locator("td").filter({ hasText: /^42$/ })).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("td").filter({ hasText: /^99$/ })).toHaveCount(0);
    } finally {
      if (dashSlug) await api.deleteDashboard(dashSlug).catch(() => {});
      await api.deleteVisualization(viz.id).catch(() => {});
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


// Polls GET /api/runs for the newest run past afterId; "Run now" uses the runs pipeline, not the
// ad-hoc execute path.
async function latestRunResultRows(
  api: E2EApiClient,
  queryId: number,
  afterId: number,
): Promise<Record<string, unknown>[]> {
  type RunRow = { id: number; status: string; query_result_id: number | null };
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const runs = await api.rawFetch<RunRow[]>(
      `/api/runs?entity_type=query&entity_id=${queryId}`,
    );
    const latest = runs.filter((r) => r.id > afterId).sort((a, b) => b.id - a.id)[0];
    if (latest?.status === "success" && latest.query_result_id != null) {
      const { query_result } = await api.rawFetch<{
        query_result: { data: { rows: Record<string, unknown>[] } };
      }>(`/api/query_results/${latest.query_result_id}`);
      return query_result.data.rows;
    }
    if (latest?.status === "failed") throw new Error("run failed");
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error("no successful run appeared within timeout");
}

test("Run now serves the published version, not a pending draft edit", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-refresh-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1 AS val" });
    try {
      await page.goto(`/queries/${q.id}`);
      await publishViaUi(page, "Publish");

      // "Run now" lives in the expanded Refresh schedule section once the query is published.
      await page.getByRole("button", { name: "Refresh schedule" }).click();
      const runNowBtn = page.getByRole("button", { name: "Run now" });

      let lastRunId = 0;
      await runNowBtn.click();
      let rows = await latestRunResultRows(api, q.id, lastRunId);
      expect(rows).toEqual([{ val: 1 }]);
      lastRunId = (
        await api.rawFetch<{ id: number }[]>(`/api/runs?entity_type=query&entity_id=${q.id}`)
      ).reduce((max, r) => Math.max(max, r.id), 0);

      await typeSql(page, "SELECT 999 AS val");
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });
      await expect(page.getByText("Unpublished changes")).toBeVisible({ timeout: 8000 });

      await runNowBtn.click();
      rows = await latestRunResultRows(api, q.id, lastRunId);
      expect(rows).toEqual([{ val: 1 }]);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("a never-published query cannot be scheduled", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `ec-neverpub-schedule-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await expect(page.getByText("Draft", { exact: true })).toBeVisible({ timeout: 8000 });

      const toggle = page.getByRole("button", { name: "Refresh schedule" });
      await expect(toggle.getByText("Publish to enable")).toBeVisible({ timeout: 8000 });
      await toggle.click();
      await expect(page.getByText("Publish this query to schedule refreshes.")).toBeVisible();
      await expect(page.getByRole("switch")).toBeDisabled();

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
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("concurrent edit: the stale writer gets the 409 reload banner, not a silent overwrite", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx1 = await adminContext(browser);
  const page1 = await ctx1.newPage();
  const ctx2 = await adminContext(browser);
  const page2 = await ctx2.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-conflict-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
    try {
      await page1.goto(`/queries/${q.id}`);
      await page2.goto(`/queries/${q.id}`);

      const fromOne = `${name}-from-1`;
      await page1.fill("#query-name", fromOne);
      await expect(page1.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });

      // page2's stale version token makes its autosave 409.
      const fromTwo = `${name}-from-2`;
      await page2.fill("#query-name", fromTwo);
      await expect(
        page2.getByText("This query was changed by someone else. Reload to get their version."),
      ).toBeVisible({ timeout: 8000 });

      // The local buffer isn't clobbered until Reload.
      await expect(page2.locator("#query-name")).toHaveValue(fromTwo);

      await page2.getByRole("button", { name: "Reload", exact: true }).click();
      await expect(page2.locator("#query-name")).toHaveValue(fromOne, { timeout: 8000 });

      const row = await getQuery(fromOne);
      expect(row).not.toBeNull();

      await expect(page2.getByText("Conflict — reload to continue")).toHaveCount(0);
      const resumedName = `${fromOne}-resumed`;
      await page2.fill("#query-name", resumedName);
      await expect(page2.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });

      await page2.getByRole("button", { name: "Back to queries" }).click();
      await expect(page2.getByText("Leave without saving?")).toHaveCount(0);
      await expect(page2).toHaveURL(/\/queries$/);
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx1.close();
  await ctx2.close();
});


test("an edit typed during an in-flight autosave is not dropped", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-midflight-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1" });
    try {
      await page.goto(`/queries/${q.id}`);

      // Hold every autosave PATCH ~2s so a second edit lands while the first is in flight.
      await page.route(`**/api/queries/${q.id}`, async (route) => {
        if (route.request().method() === "POST") {
          await new Promise((r) => setTimeout(r, 2000));
        }
        await route.continue();
      });

      const editA = `${name}-A`;
      await page.fill("#query-name", editA);
      await expect(page.getByText("Saving…", { exact: true })).toBeVisible({ timeout: 8000 });

      const editB = `${name}-B`;
      await page.fill("#query-name", editB);

      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 15_000 });

      await page.reload();
      await expect(page.locator("#query-name")).toHaveValue(editB, { timeout: 8000 });
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});


test("discard is final: a stale Retry can't resurrect a discarded draft", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  await withTargetDs(api, async (dsId) => {
    const name = `ec-discard-final-${Date.now()}`;
    const q = await api.createQuery({ data_source_id: dsId, name, query: "SELECT 1 AS val" });
    try {
      await page.goto(`/queries/${q.id}`);
      await publishViaUi(page, "Publish");

      // "Discard changes" needs a server draft differing from published; bank one first.
      await typeSql(page, "SELECT 1.5 AS val");
      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });

      // Fail every later autosave PATCH so the next edit sits in "Save failed" with Retry; /discard
      // is a different path.
      await page.route(`**/api/queries/${q.id}`, async (route) => {
        if (route.request().method() === "POST") {
          await route.fulfill({ status: 500, body: "boom" });
          return;
        }
        await route.continue();
      });

      await typeSql(page, "SELECT 2 AS val");
      // Not exact: "Save failed" sits beside a Retry button in the same element.
      await expect(page.getByText("Save failed")).toBeVisible({ timeout: 8000 });

      await clickActionBarItem(page, "Discard changes");
      const dialog = page.locator("[role=dialog]");
      await expect(dialog).toBeVisible({ timeout: 8000 });
      await dialog.getByRole("button", { name: "Discard changes", exact: true }).click();
      await expect(dialog).not.toBeVisible({ timeout: 8000 });

      // Fully gone, so nothing is left for a stale Retry to resend.
      await expect(page.getByText("Save failed")).toHaveCount(0);
      await expect(page.getByRole("button", { name: "Retry" })).toHaveCount(0);
      await expect(page.locator(".cm-content")).toContainText("SELECT 1 AS val");

      await page.reload();
      await expect(page.locator(".cm-content")).toContainText("SELECT 1 AS val", { timeout: 8000 });
      await expect(page.locator(".cm-content")).not.toContainText("SELECT 1.5");
      await expect(page.locator(".cm-content")).not.toContainText("SELECT 2");
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  });

  await ctx.close();
});
