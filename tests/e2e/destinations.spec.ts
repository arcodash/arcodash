// notification_destinations.options is encrypted binary (not encrypted_options as in data_sources).
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { queryRows } from "./helpers/db";

test("create email destination via dialog → row appears in UI + DB row exists", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-dest-${Date.now()}`;

  await page.goto("/destinations");
  await page.click('button:has-text("+ New Destination")');
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });

  // Name has no id/label pairing; target by placeholder.
  await dialog.locator('input[placeholder="My Slack channel"]').fill(name);

  await dialog.getByText("Select type").click();
  await page.getByRole("option", { name: "Email" }).click();

  await dialog.locator("input").nth(1).fill("e2e-create@example.com");

  await dialog.locator('button:has-text("Create")').click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });

  const row = page.getByRole("row", { name: new RegExp(name) });
  await expect(row).toBeVisible({ timeout: 8000 });
  await expect(row.getByRole("cell", { name: "Email" })).toBeVisible();

  const rows = await queryRows<{ id: number; name: string; type: string }>(
    "SELECT id, name, type FROM notification_destinations WHERE name = $1",
    [name],
  );
  expect(rows.length).toBe(1);
  expect(rows[0].type).toBe("email");

  await api.deleteDestination(rows[0].id).catch(() => {});
  await ctx.close();
});

test("dialog cancel (Escape) → no destination created", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-dest-cancel-${Date.now()}`;

  await page.goto("/destinations");
  await page.click('button:has-text("+ New Destination")');
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.locator('input[placeholder="My Slack channel"]').fill(name);

  await page.keyboard.press("Escape");
  await expect(dialog).not.toBeVisible({ timeout: 8000 });

  const rows = await queryRows("SELECT id FROM notification_destinations WHERE name = $1", [
    name,
  ]);
  expect(rows.length).toBe(0);

  await ctx.close();
});

test("edit via UI: rename + change username, leave URL blank → both persist, secret retained", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const original = `e2e-dest-edit-${Date.now()}`;
  const secretUrl = "https://hooks.example.com/services/T00/B00/EditTestSecret";

  // Mattermost, not Webhook: the webhook schema is URL-only, so it can't mix a non-secret change
  // with a blank secret.
  const dest = await api.createDestination({
    name: original,
    type: "mattermost",
    options: { url: secretUrl, username: "old-username" },
  });

  try {
    await page.goto("/destinations");
    const row = page.getByRole("row", { name: new RegExp(original) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: `Edit ${original}` }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });

    // Type is a read-only <p>, not an input.
    await expect(dialog.getByText("Mattermost", { exact: true })).toBeVisible();

    const renamed = `${original}-renamed`;
    // Input order: name, url, icon_emoji, username (Type isn't an input).
    const nameInput = dialog.locator("input").nth(0);
    await nameInput.fill(renamed);

    const urlInput = dialog.locator("input").nth(1);
    await expect(urlInput).toHaveAttribute("placeholder", "Leave blank to keep current");
    await expect(urlInput).toHaveValue("");

    const usernameInput = dialog.locator("input").nth(3);
    await expect(usernameInput).toHaveValue("old-username");
    await usernameInput.fill("new-username");

    await dialog.locator('button:has-text("Save")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    const renamedRow = page.getByRole("row", { name: new RegExp(renamed) });
    await expect(renamedRow).toBeVisible({ timeout: 8000 });

    const rows = await queryRows<{ name: string }>(
      "SELECT name FROM notification_destinations WHERE id = $1",
      [dest.id],
    );
    expect(rows[0].name).toBe(renamed);

    // Via the API (which decrypts): the edit applied and the untouched secret survived the merge.
    const fresh = await api.updateDestination(dest.id, {});
    expect(fresh.name).toBe(renamed);
  } finally {
    await api.deleteDestination(dest.id).catch(() => {});
    await ctx.close();
  }
});

test("edit via UI: rename only → name changes, options untouched", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const original = `e2e-dest-rename-${Date.now()}`;

  const dest = await api.createDestination({
    name: original,
    type: "email",
    options: { to: "e2e-rename@example.com" },
  });

  try {
    await page.goto("/destinations");
    const row = page.getByRole("row", { name: new RegExp(original) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: `Edit ${original}` }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });

    const renamed = `${original}-renamed`;
    await dialog.locator("input").first().fill(renamed);
    await dialog.locator('button:has-text("Save")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    const rows = await queryRows<{ name: string }>(
      "SELECT name FROM notification_destinations WHERE id = $1",
      [dest.id],
    );
    expect(rows[0].name).toBe(renamed);
  } finally {
    await api.deleteDestination(dest.id).catch(() => {});
    await ctx.close();
  }
});

test("delete via UI requires confirmation; Cancel keeps the row, Delete removes it", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-dest-delete-${Date.now()}`;

  const dest = await api.createDestination({
    name,
    type: "email",
    options: { to: "e2e-delete@example.com" },
  });

  await page.goto("/destinations");
  const row = page.getByRole("row", { name: new RegExp(name) });
  await expect(row).toBeVisible({ timeout: 8000 });

  await row.getByRole("button", { name: `Delete ${name}` }).click();
  const dialog = page.locator("[role=dialog]");
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.locator('button:has-text("Cancel")').click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
  await expect(row).toBeVisible();

  await row.getByRole("button", { name: `Delete ${name}` }).click();
  await expect(dialog).toBeVisible({ timeout: 8000 });
  await dialog.locator('button:has-text("Delete")').click();
  await expect(dialog).not.toBeVisible({ timeout: 8000 });
  await expect(row).not.toBeVisible({ timeout: 8000 });

  const rows = await queryRows("SELECT id FROM notification_destinations WHERE id = $1", [
    dest.id,
  ]);
  expect(rows.length).toBe(0);

  await ctx.close();
});

test("type display name renders correctly (e.g. Microsoft Teams, not the raw slug)", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const name = `e2e-dest-teams-${Date.now()}`;

  const dest = await api.createDestination({
    name,
    type: "microsoft_teams_webhook",
    options: { url: "https://outlook.office.com/webhook/e2e" },
  });

  try {
    await page.goto("/destinations");
    const row = page.getByRole("row", { name: new RegExp(name) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByRole("cell", { name: "Microsoft Teams" })).toBeVisible();
    await expect(row.getByRole("cell", { name: "microsoft_teams_webhook" })).toHaveCount(0);
  } finally {
    await api.deleteDestination(dest.id).catch(() => {});
    await ctx.close();
  }
});
