// Snippet bodies avoid `${n:placeholder}` tab-stops so insertion is a plain literal-text assertion.
import { test, expect, type Locator, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { queryRows } from "./helpers/db";

async function openManageDialog(page: Page) {
  // exact: true, or "Snippets" also substring-matches "Manage snippets…".
  await page.getByRole("button", { name: "Snippets", exact: true }).click();
  await page.getByRole("button", { name: "Manage snippets…" }).click();
  await expect(page.getByRole("dialog").getByText("Manage snippets")).toBeVisible({
    timeout: 5000,
  });
}

function manageRow(page: Page, trigger: string): Locator {
  return page
    .getByRole("dialog")
    .locator("div.flex.items-center.justify-between", { has: page.getByText(trigger, { exact: true }) });
}

test("create snippet → in list + DB; insert into editor → SQL text appears; edit + delete reflected", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-snip-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-snip-q-${Date.now()}`,
    query: "SELECT 1",
  });

  const trigger = `e2e-snip-${Date.now()}`;
  const body = "SELECT 1 AS e2e_snip_value";

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto(`/queries/${q.id}`);

    await openManageDialog(page);
    await page.getByRole("button", { name: "New snippet" }).click();
    await page.fill("#snippet-trigger", trigger);
    await page.fill("#snippet-description", "E2E test snippet");
    await page.fill("#snippet-body", body);
    await page.getByRole("button", { name: /^(Save|Saving…)$/ }).click();

    await expect(manageRow(page, trigger)).toBeVisible({ timeout: 8000 });

    let rows = await queryRows<{ trigger: string; snippet: string }>(
      "SELECT trigger, snippet FROM query_snippets WHERE trigger = $1",
      [trigger],
    );
    expect(rows.length).toBe(1);
    expect(rows[0].snippet).toBe(body);
    const snippetId = (
      await queryRows<{ id: number }>("SELECT id FROM query_snippets WHERE trigger = $1", [
        trigger,
      ])
    )[0].id;

    await page.keyboard.press("Escape");

    await page.getByRole("button", { name: "Snippets" }).click();
    await page.getByRole("button", { name: trigger }).click();
    await expect(page.locator(".cm-content")).toContainText(body, { timeout: 5000 });

    const editedTrigger = `${trigger}-edited`;
    const editedBody = "SELECT 2 AS e2e_snip_value_edited";
    await openManageDialog(page);
    await manageRow(page, trigger).locator("button").first().click(); // Pencil (edit)
    await expect(page.locator("#snippet-trigger")).toHaveValue(trigger, { timeout: 5000 });
    await page.fill("#snippet-trigger", editedTrigger);
    await page.fill("#snippet-body", editedBody);
    await page.getByRole("button", { name: /^(Save|Saving…)$/ }).click();

    await expect(manageRow(page, editedTrigger)).toBeVisible({ timeout: 8000 });
    rows = await queryRows<{ trigger: string; snippet: string }>(
      "SELECT trigger, snippet FROM query_snippets WHERE id = $1",
      [snippetId],
    );
    expect(rows[0].trigger).toBe(editedTrigger);
    expect(rows[0].snippet).toBe(editedBody);

    await manageRow(page, editedTrigger).locator("button").nth(1).click(); // Trash2 (delete)
    await expect(manageRow(page, editedTrigger)).not.toBeVisible({ timeout: 8000 });
    rows = await queryRows("SELECT id FROM query_snippets WHERE id = $1", [snippetId]);
    expect(rows.length).toBe(0);
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("typing an existing snippet's trigger letter by letter opens the autocomplete popup", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-snip-trigger-ds-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `e2e-snip-trigger-q-${Date.now()}`,
    query: "",
  });

  const trigger = `top${Date.now().toString().slice(-6)}`;
  const snippet = await api.createSnippet({
    trigger,
    snippet: "SELECT * FROM ${1:table} ORDER BY ${2:count} DESC LIMIT 10",
    description: "Top 10 by count",
  });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    await page.goto(`/queries/${q.id}`);
    await page.locator(".cm-content").click();
    // Type the trigger char by char so CodeMirror's autocomplete activates (fill() wouldn't).
    await page.keyboard.type(trigger, { delay: 50 });

    const popup = page.locator(".cm-tooltip.cm-tooltip-autocomplete");
    await expect(popup).toBeVisible({ timeout: 5000 });
    await expect(popup.getByText(trigger, { exact: true })).toBeVisible();

    // CodeMirror's acceptCompletion ignores Enter for 75ms after the popup updates; committing too
    // fast is an automation trap.
    await page.waitForTimeout(150);

    await page.keyboard.press("Enter");
    await expect(page.locator(".cm-content")).toContainText("SELECT * FROM table ORDER BY count DESC LIMIT 10");
  } finally {
    await api.deleteSnippet(snippet.id).catch(() => {});
    await api.deleteQuery(q.id).catch(() => {});
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
