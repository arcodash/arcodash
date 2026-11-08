// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin } from "./helpers/api";
import { queryRows } from "./helpers/db";

const TARGET_DS_OPTIONS = {
  host: "localhost",
  port: 5434,
  dbname: "target_db",
  user: "target",
  password: "target-secret-pw",
};

test("edit page is prefilled; rename + save shows pending state; detail reflects new name; encrypted_options never leaks the plaintext password", async ({
  browser,
}) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-ds-edit-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const newName = `${ds.name}-renamed`;

  try {
    await page.goto(`/data_sources/${ds.id}/edit`);

    await expect(page.locator("#ds-name")).toHaveValue(ds.name, { timeout: 8000 });
    await expect(page.locator("#ds-type")).toBeDisabled();
    await expect(page.locator("#ds-field-password")).toHaveValue("");
    await expect(page.locator("#ds-field-host")).toHaveValue(TARGET_DS_OPTIONS.host);

    await page.fill("#ds-name", newName);

    await page.route(`**/api/data_sources/${ds.id}`, async (route) => {
      if (route.request().method() === "POST") {
        await new Promise((r) => setTimeout(r, 800));
      }
      await route.continue();
    });

    const saveBtn = page.locator('button[type="submit"]');
    await saveBtn.click();
    await expect(saveBtn).toHaveText("Saving…");
    await expect(saveBtn).toBeDisabled();

    await page.waitForURL(new RegExp(`/data_sources/${ds.id}$`), { timeout: 10_000 });
    await expect(page.locator("h1")).toHaveText(newName, { timeout: 8000 });

    const rows = await queryRows<{ name: string; encrypted_options: Buffer | string }>(
      "SELECT name, encrypted_options FROM data_sources WHERE id = $1",
      [ds.id],
    );
    expect(rows[0].name).toBe(newName);
    const raw = rows[0].encrypted_options;
    const asString = Buffer.isBuffer(raw) ? raw.toString("utf8") : String(raw);
    expect(asString).not.toContain(TARGET_DS_OPTIONS.password);
    const asHex = Buffer.isBuffer(raw) ? raw.toString("hex") : Buffer.from(String(raw)).toString("hex");
    expect(asHex).not.toContain(Buffer.from(TARGET_DS_OPTIONS.password).toString("hex"));
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});

test("bad port → Test connection shows a graceful error, no crash", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  const ds = await api.createDataSource({
    name: `e2e-ds-badport-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  try {
    await page.goto(`/data_sources/${ds.id}/edit`);
    await page.fill('input[id*="port"]', "1"); // nothing listens on port 1

    await page.click('button:has-text("Test connection")');
    // TestButton's error is `text-sm text-destructive`; form "*" markers use `text-destructive`
    // alone, so plain `.text-destructive` is ambiguous.
    await expect(page.locator("span.text-sm.text-destructive")).toBeVisible({ timeout: 15_000 });

    await expect(page.locator("#ds-name")).toBeVisible();
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
    await ctx.close();
  }
});
