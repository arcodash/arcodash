/** E2E: /users admin page + authz boundaries, including two-step API key regeneration (DB
 * assertions prove the old key stops working). */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { E2EApiClient, loginAsAdmin } from "./helpers/api";
import { getUser, queryRows } from "./helpers/db";

async function inviteAndActivate(
  adminApi: E2EApiClient,
  email: string,
  password = "TestPass123!",
) {
  const invited = await adminApi.inviteUser({ name: "E2E User", email });
  const rows = await queryRows<{ token: string }>(
    "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
    [invited.id],
  );
  const token = rows[0]?.token;
  if (!token) throw new Error("invitation_token missing from DB");
  const nonAdminApi = new E2EApiClient();
  await nonAdminApi.acceptInvitation(token, "E2E User", password);
  await nonAdminApi.login(email, password);
  return { id: invited.id, api: nonAdminApi };
}

test("disable via UI requires confirmation; Cancel keeps the user active, Disable disables + 401s login", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-disable-${Date.now()}@example.com`;
  const { id, api: nonAdminApi } = await inviteAndActivate(adminApi, email);

  const loginStatus1 = await nonAdminApi.getStatus("/api/session");
  expect(loginStatus1).toBe(200);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Active")).toBeVisible();

    await row.getByRole("button", { name: "Disable" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await expect(dialog.getByText("Disable user?")).toBeVisible();
    await dialog.locator('button:has-text("Cancel")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Active")).toBeVisible();

    await row.getByRole("button", { name: "Disable" }).click();
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await dialog.locator('button:has-text("Disable")').click();
    await expect(row.getByText("Disabled")).toBeVisible({ timeout: 8000 });

    const dbUser = await getUser(email);
    expect(dbUser.disabled_at).not.toBeNull();

    const freshLogin = new E2EApiClient();
    await expect(freshLogin.login(email, "TestPass123!")).rejects.toThrow(/401/);
  } finally {
    await adminApi.deleteUser(id).catch(() => {});
    await ctx.close();
  }
});

test("own row has no Disable button", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  const adminEmail = process.env.ADMIN_EMAIL ?? "admin@example.com";

  await page.goto("/users");
  const row = page.getByRole("row", { name: new RegExp(adminEmail) });
  await expect(row).toBeVisible({ timeout: 8000 });
  await expect(row.getByRole("button", { name: "Disable" })).toHaveCount(0);

  await ctx.close();
});

test("enable via UI: a disabled row shows only Enable, and clicking it flips the user back to Active", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-enable-${Date.now()}@example.com`;
  const { id } = await inviteAndActivate(adminApi, email);
  await adminApi.deleteUser(id); // soft-disable

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Disabled")).toBeVisible();
    await expect(row.getByRole("button", { name: "Edit" })).toHaveCount(0);

    await row.getByRole("button", { name: "Enable" }).click();
    await expect(row.getByText("Active")).toBeVisible({ timeout: 8000 });

    const dbUser = await getUser(email);
    expect(dbUser.disabled_at).toBeNull();
  } finally {
    const rows = await queryRows<{ id: number }>("SELECT id FROM users WHERE email = $1", [
      email,
    ]);
    if (rows[0]) await adminApi.deleteUser(rows[0].id).catch(() => {});
    await ctx.close();
  }
});

test("edit name via UI dialog persists to the DB", async ({ browser }) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-rename-${Date.now()}@example.com`;
  const { id } = await inviteAndActivate(adminApi, email);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: "Edit" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });

    const newName = "E2E Renamed User";
    const nameInput = dialog.locator("input").first();
    await nameInput.fill(newName);
    await dialog.locator('button:has-text("Save")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    const dbUser = await getUser(email);
    expect(dbUser.name).toBe(newName);
  } finally {
    await adminApi.deleteUser(id).catch(() => {});
    await ctx.close();
  }
});

test("regenerate API key via UI: confirm -> new key shown -> old key 401s, new key works", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-regen-${Date.now()}@example.com`;
  const { id } = await inviteAndActivate(adminApi, email);

  // Keys are stored hashed; the regenerate response is the only place to read plaintext, so call it
  // via the API to get a known "old" key.
  const { api_key: oldKey } = await adminApi.rawFetch<{ api_key: string }>(
    `/api/users/${id}/regenerate_api_key`,
    { method: "POST" },
  );

  // Use /api/queries (401 without CurrentUser); /api/session is always 200, so it can't distinguish
  // a valid key.
  async function statusWithKey(key: string): Promise<number> {
    const res = await fetch(`${process.env.BACKEND_URL ?? "http://localhost:8080"}/api/queries`, {
      headers: { Authorization: `Key ${key}` },
    });
    return res.status;
  }

  expect(await statusWithKey(oldKey)).toBe(200);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: "Regenerate key" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await expect(dialog.getByText("Regenerate API key?")).toBeVisible();
    await dialog.locator('button:has-text("Regenerate")').click();

    await expect(dialog.getByText("New API key")).toBeVisible({ timeout: 8000 });
    const keyInput = dialog.locator("input[readonly]");
    const newKey = await keyInput.inputValue();
    expect(newKey).not.toBe(oldKey);
    expect(newKey.length).toBeGreaterThan(0);

    await dialog.locator('button:has-text("Done")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });

    expect(await statusWithKey(oldKey)).toBe(401);
    expect(await statusWithKey(newKey)).toBe(200);
  } finally {
    await adminApi.deleteUser(id).catch(() => {});
    await ctx.close();
  }
});

test("search filters by name/email fragment; no match shows an empty state", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const uniqueFragment = `e2e-search-${Date.now()}`;
  const email = `${uniqueFragment}@example.com`;
  const { id } = await inviteAndActivate(adminApi, email);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });

    const search = page.getByLabel("Search users");
    await search.fill(uniqueFragment);
    await expect(row).toBeVisible();

    await search.fill("no-such-user-fragment-xyz");
    await expect(page.getByText("No users match your search.")).toBeVisible({ timeout: 8000 });
  } finally {
    await adminApi.deleteUser(id).catch(() => {});
    await ctx.close();
  }
});

test("a pending-invitation row offers Edit, Resend invitation, and Revoke — not Enable", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-pending-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Pending", email });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("Invited")).toBeVisible();

    await expect(row.getByRole("button", { name: "Edit" })).toBeVisible();
    await expect(row.getByRole("button", { name: "Resend invitation" })).toBeVisible();
    await expect(row.getByRole("button", { name: "Revoke" })).toBeVisible();
    await expect(row.getByRole("button", { name: "Enable" })).toHaveCount(0);
    await expect(row.getByRole("button", { name: "Regenerate key" })).toHaveCount(0);
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
    await ctx.close();
  }
});

test("resend invitation via UI regenerates the token; the old one stops working and the new one accepts", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-resend-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Resend", email });

  const oldTokenRows = await queryRows<{ token: string }>(
    "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
    [invited.id],
  );
  const oldToken = oldTokenRows[0]?.token;
  if (!oldToken) throw new Error("invitation_token missing from DB");

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: "Resend invitation" }).click();
    await expect(row.getByRole("button", { name: "Resend invitation" })).toBeEnabled({
      timeout: 8000,
    });

    const newTokenRows = await queryRows<{ token: string }>(
      "SELECT details->>'invitation_token' AS token FROM users WHERE id = $1",
      [invited.id],
    );
    const newToken = newTokenRows[0]?.token;
    expect(newToken).toBeTruthy();
    expect(newToken).not.toBe(oldToken);

    const nonAdminApi = new E2EApiClient();
    await expect(nonAdminApi.acceptInvitation(oldToken, "Too Late", "Wh4tever!Pass")).rejects.toThrow(
      /400/,
    );
    await nonAdminApi.acceptInvitation(newToken!, "E2E Resend", "Acc3pt3d!Pass");
  } finally {
    await adminApi.deleteUser(invited.id).catch(() => {});
    await ctx.close();
  }
});

test("revoke invitation via UI requires confirmation and removes the pending row entirely", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-revoke-${Date.now()}@example.com`;
  const invited = await adminApi.inviteUser({ name: "E2E Revoke", email });

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });

    await row.getByRole("button", { name: "Revoke" }).click();
    const dialog = page.locator("[role=dialog]");
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await expect(dialog.getByText("Revoke invitation?")).toBeVisible();
    await dialog.locator('button:has-text("Cancel")').click();
    await expect(dialog).not.toBeVisible({ timeout: 8000 });
    await expect(row).toBeVisible();

    await row.getByRole("button", { name: "Revoke" }).click();
    await expect(dialog).toBeVisible({ timeout: 8000 });
    await dialog.locator('button:has-text("Revoke")').click();
    await expect(row).toHaveCount(0, { timeout: 8000 });

    // Poll the DB: the delete's commit may not yet be visible on a new connection when the UI
    // refetch settles.
    await expect
      .poll(
        async () =>
          (
            await queryRows<{ id: number }>("SELECT id FROM users WHERE id = $1", [invited.id])
          ).length,
        { timeout: 8000 },
      )
      .toBe(0);
  } finally {
    await ctx.close();
  }
});

test("a group id referencing a deleted group renders a placeholder chip, not a raw id", async ({
  browser,
}) => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-unknown-group-${Date.now()}@example.com`;
  const { id } = await inviteAndActivate(adminApi, email);
  const group = await adminApi.createGroup({ name: `e2e-doomed-group-${Date.now()}` });
  await adminApi.rawFetch(`/api/users/${id}`, {
    method: "POST",
    body: JSON.stringify({ groups: [group.id] }),
  });
  await adminApi.deleteGroup(group.id);
  // The user must still reference the deleted group, or the row below proves nothing.
  const stored = await adminApi.rawFetch<{ groups: number[] }>(`/api/users/${id}`);
  expect(stored.groups).toContain(group.id);

  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.goto("/users");
    const row = page.getByRole("row", { name: new RegExp(email) });
    await expect(row).toBeVisible({ timeout: 8000 });
    await expect(row.getByText("—", { exact: true })).toBeVisible();
    await expect(row.getByText(`#${group.id}`)).toHaveCount(0);
    await expect(row.getByText(String(group.id), { exact: true })).toHaveCount(0);
  } finally {
    await adminApi.deleteUser(id).catch(() => {});
    await ctx.close();
  }
});

test("authz: non-admin gets 403 updating another user or regenerating their key", async () => {
  const adminApi = await loginAsAdmin();
  const email = `e2e-users-nonadmin-${Date.now()}@example.com`;
  const { id: nonAdminId, api: nonAdminApi } = await inviteAndActivate(adminApi, email);

  const otherEmail = `e2e-users-victim-${Date.now()}@example.com`;
  const { id: otherId } = await inviteAndActivate(adminApi, otherEmail);

  try {
    await expect(
      nonAdminApi.rawFetch(`/api/users/${otherId}`, {
        method: "POST",
        body: JSON.stringify({ name: "Hacked" }),
      }),
    ).rejects.toThrow(/403/);

    await expect(
      nonAdminApi.rawFetch(`/api/users/${otherId}/regenerate_api_key`, { method: "POST" }),
    ).rejects.toThrow(/403/);
  } finally {
    await adminApi.deleteUser(nonAdminId).catch(() => {});
    await adminApi.deleteUser(otherId).catch(() => {});
  }
});
