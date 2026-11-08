import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";
import { delKey } from "./helpers/valkey";
import { Client } from "pg";
import { seedCustomPermissionUser } from "./helpers/permissions";



test("create postgres data source via UI → appears in list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const dsName = `e2e-ds-${Date.now()}`;

  await page.goto("/data_sources/new");

  await page.fill("#ds-name", dsName);

  await page.click("#ds-type");
  await page.getByRole("option", { name: "PostgreSQL" }).click();

  await page.fill('input[id*="host"]', TARGET_DS_OPTIONS.host);
  const portInput = page.locator('input[id*="port"]');
  if (await portInput.count() > 0) {
    await portInput.fill(String(TARGET_DS_OPTIONS.port));
  }
  await page.fill('input[id*="dbname"]', TARGET_DS_OPTIONS.dbname);
  await page.fill('input[id*="user"]', TARGET_DS_OPTIONS.user);
  await page.fill('input[id*="password"]', TARGET_DS_OPTIONS.password);

  await page.click('button[type="submit"]');

  await page.waitForURL(/\/data_sources\/\d+/, { timeout: 10_000 });
  await expect(page.getByText(dsName)).toBeVisible();

  const rows = await queryRows<{ id: number }>(
    "SELECT id FROM data_sources WHERE name = $1",
    [dsName],
  );
  if (rows[0]) await api.deleteDataSource(rows[0].id).catch(() => {});
  await ctx.close();
});

test("test connection → shows success", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-conn-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await page.click('button:has-text("Test connection")');
    await expect(page.locator(".text-green-600")).toBeVisible({ timeout: 15_000 });
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("list data sources → newly created one appears", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-list-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto("/data_sources");
    await expect(page.getByText(ds.name)).toBeVisible({ timeout: 8000 });
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("DB assertion: data_sources row created with encrypted_options", async () => {
  const api = await loginAsAdmin();
  const dsName = `e2e-dbcheck-${Date.now()}`;
  const ds = await api.createDataSource({
    name: dsName,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    const rows = await queryRows<{ name: string; encrypted_options: string }>(
      "SELECT name, encrypted_options FROM data_sources WHERE id = $1",
      [ds.id],
    );
    expect(rows.length).toBe(1);
    expect(rows[0].name).toBe(dsName);
    expect(rows[0].encrypted_options).not.toContain(TARGET_DS_OPTIONS.password);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

test("schema browser lists target table after selecting data source", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-schema-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByRole("button", { name: "typed_table" })).toBeVisible({ timeout: 15_000 });
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("turn off → indicator shows Off; turn on → indicator shows On", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-pause-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    // Create probes a reachable target (data_source_service::probe_health_on_save), so health
    // already reads "On · Healthy".
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });

    await page.click('button:has-text("Turn off")');
    await expect(page.getByText("Off", { exact: true })).toBeVisible({ timeout: 8000 });

    // Pausing sets paused_at and must never write the health columns (availability is separate from
    // health).
    const rows = await queryRows<{ paused_at: string | null }>(
      "SELECT paused_at FROM data_sources WHERE id = $1",
      [ds.id],
    );
    expect(rows[0].paused_at).not.toBeNull();

    await page.click('button:has-text("Turn on")');
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("a rejected pause surfaces a visible error and leaves the indicator On", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-pause-fail-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });

    await page.route(`**/api/data_sources/${ds.id}/pause`, (route) =>
      route.fulfill({
        status: 500,
        contentType: "application/json",
        body: JSON.stringify({ message: "Couldn't turn off this data source right now." }),
      }),
    );

    await page.click('button:has-text("Turn off")');
    await expect(
      page.getByText("Couldn't turn off this data source right now."),
    ).toBeVisible({ timeout: 8000 });
    // Rejected, not silently reverted: the indicator stays "On".
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Turn off" })).toBeVisible();

    const rows = await queryRows<{ paused_at: string | null }>(
      "SELECT paused_at FROM data_sources WHERE id = $1",
      [ds.id],
    );
    expect(rows[0].paused_at).toBeNull();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

// Pausing from the detail page must block running its queries, not just flip an indicator.
test("pausing a data source from its detail page blocks running its queries with a visible error", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-pause-blocks-run-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-pause-blocks-run-query-${Date.now()}`,
    query: "SELECT 1",
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });
    await page.click('button:has-text("Turn off")');
    await expect(page.getByText("Off", { exact: true })).toBeVisible({ timeout: 8000 });

    await page.goto(`/queries/${q.id}`);
    await page.getByRole("button", { name: /execute/i }).click();
    await expect(page.getByText(/paused/i)).toBeVisible({ timeout: 10_000 });

    await page.goto(`/data_sources/${ds.id}`);
    await page.click('button:has-text("Turn on")');
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });

    await page.goto(`/queries/${q.id}`);
    await page.getByRole("button", { name: /execute/i }).click();
    await expect(page.getByText(/paused/i)).not.toBeVisible({ timeout: 5000 });
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("data source health: create probes it OK immediately, and a real query run keeps it OK", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-health-ok-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  let queryId: number | undefined;

  try {
    // Create records the connectivity probe result immediately; health is never left "unknown".
    const rows = await queryRows<{ health_state: string }>(
      "SELECT health_state FROM data_sources WHERE id = $1",
      [ds.id],
    );
    expect(rows[0].health_state).toBe("ok");

    // A real query's execute_query hook must keep health OK reactively, no polling.
    const query = await api.createQuery({
      name: `e2e-health-ok-query-${Date.now()}`,
      data_source_id: ds.id,
      query: "SELECT 1 AS n",
    });
    queryId = query.id;
    await api.executeQuery(query.id);

    await expect
      .poll(
        async () => {
          const r = await queryRows<{ health_state: string }>(
            "SELECT health_state FROM data_sources WHERE id = $1",
            [ds.id],
          );
          return r[0]?.health_state;
        },
        { timeout: 15_000 },
      )
      .toBe("ok");

    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByText("On · Healthy", { exact: true })).toBeVisible({ timeout: 8000 });
  } finally {
    // A data source with a referencing query can't be deleted — clear the query first.
    if (queryId != null) await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("data source health: a query run that fails to connect marks it Failing, with a reachable error surfaced", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-health-failing-${Date.now()}`,
    type: "pg",
    options: { ...TARGET_DS_OPTIONS, port: 1 },
  });
  let queryId: number | undefined;

  try {
    const query = await api.createQuery({
      name: `e2e-health-failing-query-${Date.now()}`,
      data_source_id: ds.id,
      query: "SELECT 1",
    });
    queryId = query.id;
    await api.executeQuery(query.id).catch(() => {});

    await expect
      .poll(
        async () => {
          const r = await queryRows<{ health_state: string }>(
            "SELECT health_state FROM data_sources WHERE id = $1",
            [ds.id],
          );
          return r[0]?.health_state;
        },
        { timeout: 15_000 },
      )
      .toBe("failing");

    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByText("On · Unreachable")).toBeVisible({ timeout: 8000 });
    await expect(page.getByText(/Last checked/i)).toBeVisible();
  } finally {
    if (queryId != null) await api.deleteQuery(queryId).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("data source detail page: Edit navigates to the edit form, Delete removes it and returns to the list", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-detail-edit-delete-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await page.getByRole("link", { name: /^edit$/i }).click();
    await page.waitForURL(`**/data_sources/${ds.id}/edit`);

    await page.goto(`/data_sources/${ds.id}`);
    await page.getByRole("button", { name: /^delete$/i }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText(/permanently delete/i)).toBeVisible();
    await dialog.getByRole("button", { name: /^delete$/i }).click();

    await page.waitForURL("**/data_sources");
    const rows = await queryRows<{ id: number }>("SELECT id FROM data_sources WHERE id = $1", [ds.id]);
    expect(rows.length).toBe(0);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

// The reverse of GroupsPage's "Data sources" panel, over the same data_source_group join.
test("data source detail page: group access reflects restriction and toggling assigns/removes", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-detail-groups-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const group = await api.createGroup({ name: `e2e-detail-groups-restricted-${Date.now()}` });
  await api.setGroupDataSourceRestricted(group.id, true);

  try {
    await page.goto(`/data_sources/${ds.id}`);
    const row = page.getByText(group.name).locator("..");
    await expect(row).toBeVisible();
    const toggle = row.getByRole("switch");
    await expect(toggle).not.toBeChecked();

    await toggle.click();
    await expect(toggle).toBeChecked();
    const afterAssign = await api.listGroupDataSources(group.id);
    expect(afterAssign.entries.some((e) => e.data_source_id === ds.id)).toBe(true);

    await toggle.click();
    await expect(toggle).not.toBeChecked();
    const afterRemove = await api.listGroupDataSources(group.id);
    expect(afterRemove.entries.some((e) => e.data_source_id === ds.id)).toBe(false);
  } finally {
    await api.deleteGroup(group.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("new data source form: Type starts empty, connection fields hidden until chosen, Name placeholder is generic then type-specific", async ({
  browser,
}) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto("/data_sources/new");

    await expect(page.getByText("Select a data source type")).toBeVisible();
    await expect(page.locator('input[id*="host"]')).not.toBeVisible();
    await expect(page.locator("#ds-name")).toHaveAttribute("placeholder", "My database");

    await page.click("#ds-type");
    await page.getByRole("option", { name: "PostgreSQL" }).click();

    await expect(page.locator('input[id*="host"]')).toBeVisible();
    await expect(page.locator("#ds-name")).toHaveAttribute("placeholder", "My PostgreSQL database");
  } finally {
    await ctx.close();
  }
});

test("data source detail page: admin builds the AI profile and expands a table; a non-admin never sees the card", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-ai-profile-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const user = await seedCustomPermissionUser(api, "ai-profile", ["view_data_source"]);
  // New groups start restricted, which would hide the data source from this user.
  await api.setGroupDataSourceRestricted(user.groupId, false);

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByRole("heading", { name: "AI profile" })).toBeVisible();
    await expect(page.getByLabel("AI profile").getByText("typed_table")).toBeVisible({ timeout: 15_000 });

    const action = page.getByRole("button", { name: /^(Build|Rebuild) profile$/ });
    await action.click();
    await expect(page.getByRole("button", { name: /Updating…/ })).toBeVisible();
    await expect(page.getByText(/Ready · built just now/)).toBeVisible({ timeout: 60_000 });

    await page.getByText(/rows sampled/).first().click();
    await expect(page.getByRole("columnheader", { name: "Column" }).first()).toBeVisible();

    const userCtx = await browser.newContext();
    const userPage = await userCtx.newPage();
    try {
      await userPage.goto("/login");
      await userPage.fill("#email", user.email);
      await userPage.fill("#password", user.password);
      await userPage.click('button[type="submit"]');
      await userPage.waitForURL((u) => !u.pathname.startsWith("/login"), { timeout: 10_000 });
      await userPage.goto(`/data_sources/${ds.id}`);
      await userPage.waitForLoadState("networkidle");
      await expect(userPage.getByText(ds.name).first()).toBeVisible();
      await expect(userPage.getByRole("heading", { name: "AI profile" })).toHaveCount(0);
    } finally {
      await userCtx.close();
    }
  } finally {
    await user.cleanup();
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("data sources list: Profiling… chip shows while a profile runs, then clears; failed chip links to the profile card", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-profile-chip-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  // Flags are rewritten on the list response so the chip lifecycle is deterministic.
  let flags: { profiling?: boolean; profile_failed?: boolean } = { profiling: true };
  await page.route("**/api/data_sources", async (route) => {
    if (route.request().method() !== "GET") return route.continue();
    const res = await route.fetch();
    const json = await res.json();
    json.data_sources = json.data_sources.map((d: { id: number }) =>
      d.id === ds.id ? { ...d, profiling: false, profile_failed: false, ...flags } : d,
    );
    await route.fulfill({ response: res, json });
  });

  try {
    await page.goto("/data_sources");
    const card = page.locator("[data-slot=card]").filter({ hasText: ds.name });
    const chip = card.getByTestId("ds-profile-chip");
    await expect(chip).toHaveAttribute("data-state", "profiling");

    flags = {};
    await expect(chip).toHaveCount(0, { timeout: 10_000 });

    flags = { profile_failed: true };
    await page.reload();
    await expect(chip).toHaveAttribute("data-state", "failed");
    await chip.click();
    await expect(page).toHaveURL(new RegExp(`/data_sources/${ds.id}#ai-profile$`));
    await expect(page.locator("#ai-profile")).toBeInViewport();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("AI profile card: failure banner + Retry, unsampled tables, metered confirm (mocked API)", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const ds = await api.createDataSource({
    name: `e2e-ai-profile-states-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  const col = { name: "id", null_frac: 0, distinct_in_sample: 3, samples: ["1"] };
  const payload = {
    profile: {
      tables: [
        { name: "orders", sampled_rows: 3, columns: [col], data_profiled_at: "2026-10-04T00:00:00Z" },
        { name: "locked", sampled_rows: 0, columns: [col], error: "permission denied" },
      ],
      relationships: [],
      schema_refreshed_at_seen: "2026-10-04T00:00:00Z",
      descriptions_used_samples: false,
    },
    built_at: "2026-10-04T00:00:00Z",
    supported: true,
    building: false,
    current: true,
    phase: null,
    last_error: "connection refused" as string | null,
    metered: false,
  };
  let rebuilds = 0;
  await page.route(`**/api/data_sources/${ds.id}/ai_profile`, (route) => route.fulfill({ json: payload }));
  await page.route(`**/api/data_sources/${ds.id}/ai_profile/rebuild`, (route) => {
    rebuilds += 1;
    return route.fulfill({ status: 202, json: { status: "queued" } });
  });

  try {
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByTestId("ai-profile-failed")).toContainText("connection refused");
    await page.getByTestId("ai-profile-unsampled").getByText(/1 of 2 tables/).click();
    await expect(page.getByTestId("ai-profile-unsampled")).toContainText("locked — permission denied");
    await page.getByTestId("ai-profile-retry").click();
    await expect.poll(() => rebuilds).toBe(1);

    payload.last_error = null;
    payload.metered = true;
    await page.reload();
    await expect(page.getByTestId("ai-profile-metered-note")).toBeVisible();
    await page.getByRole("button", { name: "Rebuild profile" }).click();
    await expect(page.getByTestId("ai-profile-metered-confirm")).toBeVisible();
    await page.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByTestId("ai-profile-metered-confirm")).toBeHidden();
    expect(rebuilds).toBe(1);
    await page.getByRole("button", { name: "Rebuild profile" }).click();
    await page.getByRole("button", { name: "Run sample queries" }).click();
    await expect(page.getByTestId("ai-profile-metered-confirm")).toBeHidden();
    expect(rebuilds).toBe(2);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("schema refresh button → worker refreshes → table created out-of-band appears", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const table = `e2e_refresh_${Date.now()}`;
  const target = new Client({ connectionString: "postgres://target:target@localhost:5434/target_db" });
  await target.connect();

  const ds = await api.createDataSource({
    name: `e2e-schema-refresh-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    // Ids get reused; a stale pending key would make the enqueue a no-op.
    await delKey(`arcodash:ai_profile:pending:${ds.id}`);
    await page.goto(`/data_sources/${ds.id}`);
    await expect(page.getByRole("button", { name: "typed_table" })).toBeVisible({ timeout: 30_000 });
    // The empty-cache load queues a profile job; let it finish so the refresh below can enqueue.
    await expect
      .poll(async () => (await (await ctx.request.get(`/api/data_sources/${ds.id}/ai_profile`)).json()).building, {
        timeout: 60_000,
      })
      .toBe(false);

    await target.query(`CREATE TABLE ${table} (id int)`);
    const refreshResponse = page.waitForResponse(
      (r) => r.url().includes(`/api/data_sources/${ds.id}/schema`) && r.url().includes("refresh=true"),
    );
    await page.getByTestId("schema-refresh").click();
    expect((await (await refreshResponse).json()).refreshing).toBe(true);

    await expect(page.getByRole("button", { name: table })).toBeVisible({ timeout: 30_000 });
  } finally {
    await target.query(`DROP TABLE IF EXISTS ${table}`).catch(() => {});
    await target.end().catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
