import { expect, type Page } from "@playwright/test";

// Click a secondary ActionBar action by label, inline or in "More actions"; polls because the item
// exists only after the query fetch resolves.
 // Scoped to actionbar-root with exact: true; getByRole's substring name match would hit unrelated
 // names (strict-mode violation).
export async function clickActionBarItem(page: Page, label: string | RegExp) {
  const actionBar = page.getByTestId("actionbar-root");
  const wideButton = actionBar.getByRole("button", { name: label, exact: true });
  const moreButton = actionBar.getByRole("button", { name: "More actions" });
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    if (await wideButton.isVisible().catch(() => false)) {
      await wideButton.click();
      return;
    }
    if (await moreButton.isVisible().catch(() => false)) {
      await moreButton.click();
      await page.getByRole("menuitem", { name: label, exact: true }).click();
      return;
    }
    await page.waitForTimeout(100);
  }
  throw new Error(`clickActionBarItem: neither wide button nor "More actions" became visible for "${label}"`);
}

/** Picks `dsName` explicitly: concurrent suites share the DB, so the editor's default pick can't be
 * trusted. */
export async function selectDataSource(page: Page, dsName: string) {
  const picker = page.getByTestId("query-editor-data-source-select");
  await picker.click();
  await page.getByRole("option", { name: dsName }).click();
  await expect(picker).toContainText(dsName);
}

export async function expectNoDeadEnd(page: Page) {
  await expect(page.getByText(/you do not have permission/i)).toHaveCount(0);
  await expect(page.getByText(/^403$/)).toHaveCount(0);
}

export async function enterEditMode(page: Page) {
  await page.getByRole("button", { name: "Edit", exact: true }).click();
}
