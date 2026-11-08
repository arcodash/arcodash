/**
 * Shared CodeMirror SQL editor helpers for E2E specs.
 */
import type { Page } from "@playwright/test";

/** Replace the SQL editor's full contents by selecting all and typing — CodeMirror's
 * contenteditable `.cm-content` region, not a plain `<textarea>`. */
export async function typeSql(page: Page, sql: string) {
  await page.locator(".cm-content").click();
  await page.keyboard.press("ControlOrMeta+a");
  await page.keyboard.type(sql);
}
