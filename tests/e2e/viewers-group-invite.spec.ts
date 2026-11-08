// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { invitationToken } from "./helpers/permissions";
import { queryRows } from "./helpers/db";
import { typeSql } from "./helpers/editor";
import { selectDataSource } from "./helpers/actions";

const PASSWORD = "E2eViewersGroup123!";

async function expectNoDeadEnd(page: Page) {
  await expect(page.getByText(/you do not have permission/i)).toHaveCount(0);
}

test("invite dialog requires an explicit group pick, offers viewers least-privilege-first, and the resulting Viewer/Editor split holds", async ({
  browser,
}) => {
  test.setTimeout(60_000);
  const adminApi = await loginAsAdmin();
  const adminCtx = await adminContext(browser);
  const adminPage = await adminCtx.newPage();
  const stamp = Date.now();
  const viewerEmail = `e2e-viewers-invite-${stamp}@example.com`;
  const editorEmail = `e2e-viewers-contrast-${stamp}@example.com`;
  const ds = await adminApi.createDataSource({
    name: `e2e-viewers-group-ds-${stamp}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });

  let viewerCtx: Awaited<ReturnType<typeof browser.newContext>> | null = null;
  let editorCtx: Awaited<ReturnType<typeof browser.newContext>> | null = null;
  let viewerId: number | undefined;
  let editorId: number | undefined;
  let queryId: number | undefined;

  try {
    await adminPage.goto("/access?tab=users");
    await adminPage.getByRole("button", { name: "+ Invite User" }).click();
    // Named dialog: the closed group-picker Popover also has role="dialog" and lingers in the DOM.
    const dialog = adminPage.getByRole("dialog", { name: "Invite User" });
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await dialog.getByPlaceholder("Jane Smith").fill("E2E Viewers Invitee");
    await dialog.getByPlaceholder("jane@example.com").fill(viewerEmail);

    await expect(
      dialog.getByText("Pick at least one group — groups decide what this user can see and do."),
    ).toBeVisible({ timeout: 8000 });
    const sendBtn = dialog.getByRole("button", { name: "Send Invitation", exact: true });
    await expect(sendBtn).toBeDisabled();

    await dialog.getByRole("button", { name: "Add group…" }).click();
    // The picker is a Radix Popover portalled to <body>, so scope to the page, not the dialog.
    const popover = adminPage.locator('[data-slot="popover-content"]');
    await expect(popover).toBeVisible({ timeout: 8000 });
    // allTextContents doesn't auto-wait; rows arrive after the groups query resolves.
    await expect(popover.locator("label", { hasText: /^admin\b/ })).toBeVisible({ timeout: 8000 });
    const rowTexts = await popover.locator("label").allTextContents();
    const idxViewers = rowTexts.findIndex((t) => /^viewers\b/.test(t));
    const idxDefault = rowTexts.findIndex((t) => /^default\b/.test(t));
    const idxAdmin = rowTexts.findIndex((t) => /^admin\b/.test(t));
    expect(idxViewers, "viewers row not found in group picker").toBeGreaterThanOrEqual(0);
    expect(idxDefault, "default row not found in group picker").toBeGreaterThanOrEqual(0);
    expect(idxAdmin, "admin row not found in group picker").toBeGreaterThanOrEqual(0);
    expect(idxViewers).toBeLessThan(idxDefault);
    expect(idxDefault).toBeLessThan(idxAdmin);
    expect(rowTexts[idxViewers]).toContain("Viewer"); // annotated with the role it grants

    await adminPage.getByLabel(/^viewers\b/).click();
    await adminPage.keyboard.press("Escape"); // close the popover, not the dialog underneath it
    await expect(sendBtn).toBeEnabled();

    await sendBtn.click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    await adminPage.getByRole("button", { name: "+ Invite User" }).click();
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await dialog.getByPlaceholder("Jane Smith").fill("E2E Viewers Contrast");
    await dialog.getByPlaceholder("jane@example.com").fill(editorEmail);
    await dialog.getByRole("button", { name: "Add group…" }).click();
    await adminPage.getByPlaceholder("Search…").fill("default");
    await adminPage.getByLabel(/^default\b/).click();
    await adminPage.keyboard.press("Escape");
    await expect(sendBtn).toBeEnabled();
    await sendBtn.click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    const viewerRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      viewerEmail,
    ]);
    const editorRows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      editorEmail,
    ]);
    viewerId = viewerRows[0].id;
    editorId = editorRows[0].id;

    // The token is read straight from the DB (no SMTP in E2E).
    viewerCtx = await browser.newContext();
    const viewerPage = await viewerCtx.newPage();
    const viewerToken = await invitationToken(viewerId);
    await viewerPage.goto(`/users/invitation/accept?token=${viewerToken}`);
    await expect(viewerPage.getByText(viewerEmail)).toBeVisible({ timeout: 8000 });
    await viewerPage.fill("#inv-name", "E2E Viewers Invitee");
    await viewerPage.fill("#inv-pw", PASSWORD);
    // Password hashing is CPU-bound server-side; wait for the accept response, then the redirect.
    await Promise.all([
      viewerPage.waitForResponse((r) => r.url().includes("/api/users/invitation/accept"), { timeout: 30_000 }),
      viewerPage.getByRole("button", { name: "Create account" }).click(),
    ]);
    await viewerPage.waitForURL((url) => !url.pathname.startsWith("/users/invitation"), {
      timeout: 8000,
    });

    editorCtx = await browser.newContext();
    const editorPage = await editorCtx.newPage();
    const editorToken = await invitationToken(editorId);
    await editorPage.goto(`/users/invitation/accept?token=${editorToken}`);
    await expect(editorPage.getByText(editorEmail)).toBeVisible({ timeout: 8000 });
    await editorPage.fill("#inv-name", "E2E Viewers Contrast");
    await editorPage.fill("#inv-pw", PASSWORD);
    await Promise.all([
      editorPage.waitForResponse((r) => r.url().includes("/api/users/invitation/accept"), { timeout: 30_000 }),
      editorPage.getByRole("button", { name: "Create account" }).click(),
    ]);
    await editorPage.waitForURL((url) => !url.pathname.startsWith("/users/invitation"), {
      timeout: 8000,
    });

    await expect(viewerPage.getByRole("link", { name: "New Query" })).toHaveCount(0);

    await viewerPage.goto("/queries/new");
    await viewerPage.waitForURL((url) => url.pathname !== "/queries/new", { timeout: 8000 });
    await expect(viewerPage.locator(".cm-editor")).toHaveCount(0);
    await expectNoDeadEnd(viewerPage);

    await viewerPage.goto("/access?tab=users");
    await viewerPage.waitForURL((url) => !url.pathname.startsWith("/access"), { timeout: 8000 });
    await expect(viewerPage.getByRole("tab", { name: "Users" })).toHaveCount(0);
    await expectNoDeadEnd(viewerPage);

    await editorPage.goto("/queries/new");
    await editorPage.waitForSelector("#query-name", { timeout: 8000 });
    const queryName = `e2e-viewers-contrast-q-${stamp}`;
    await editorPage.fill("#query-name", queryName);
    await selectDataSource(editorPage, ds.name);
    await typeSql(editorPage, "SELECT 42 AS val");
    await editorPage.click('button:has-text("Execute")');
    await editorPage.waitForURL(/\/queries\/\d+/, { timeout: 10_000 });
    queryId = Number(editorPage.url().match(/\/queries\/(\d+)/)![1]);

    await expect
      .poll(
        async () => {
          const rows = await queryRows<{ last_execution_status: string }>(
            "SELECT last_execution_status FROM queries WHERE id = $1",
            [queryId],
          );
          return rows[0]?.last_execution_status;
        },
        { timeout: 10_000 },
      )
      .toBe("Success");
  } finally {
    if (queryId) await adminApi.deleteQuery(queryId).catch(() => {});
    if (viewerId) await adminApi.deleteUser(viewerId).catch(() => {});
    if (editorId) await adminApi.deleteUser(editorId).catch(() => {});
    await adminApi.deleteDataSource(ds.id).catch(() => {});
    if (viewerCtx) await viewerCtx.close();
    if (editorCtx) await editorCtx.close();
    await adminCtx.close();
  }
});
