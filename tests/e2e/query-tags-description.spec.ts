// Requires: backend :8080, frontend :3000.
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, E2EApiClient } from "./helpers/api";
import { queryRows } from "./helpers/db";

async function withDataSource(api: E2EApiClient, fn: (dsId: number) => Promise<void>) {
  const ds = await api.createDataSource({
    name: `e2e-tags-ds-${Date.now()}-${Math.random().toString(36).slice(2)}`,
    type: "pg",
    options: {},
  });
  try {
    await fn(ds.id);
  } finally {
    await api.deleteDataSource(ds.id).catch(() => {});
  }
}

async function queryTagsAndDescription(
  id: number,
): Promise<{ tags: string[] | null; description: string | null }> {
  const rows = await queryRows<{ tags: string[] | null; description: string | null }>(
    "SELECT tags, description FROM queries WHERE id = $1",
    [id],
  );
  return rows[0] ?? { tags: null, description: null };
}

test("tag round-trip through the query editor: add, reload, remove", async ({ browser }) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const tag = `e2e-round-trip-${stamp}`;

  await withDataSource(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-tag-roundtrip-${stamp}`,
      query: "SELECT 1",
    });

    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    try {
      await page.goto(`/queries/${q.id}`);
      const tagInput = page.getByPlaceholder("add tag…");
      await expect(tagInput).toBeVisible({ timeout: 8000 });
      await tagInput.fill(tag);
      await tagInput.press("Enter");

      const chip = page.getByText(tag, { exact: true });
      await expect(chip).toBeVisible({ timeout: 8000 });

      await expect
        .poll(async () => (await queryTagsAndDescription(q.id)).tags, { timeout: 8000 })
        .toEqual([tag]);

      await page.reload();
      await expect(page.getByText(tag, { exact: true })).toBeVisible({ timeout: 8000 });

      await page.getByRole("button", { name: `Remove tag ${tag}` }).click();
      await expect(page.getByText(tag, { exact: true })).not.toBeVisible({ timeout: 8000 });

      await expect
        .poll(async () => (await queryTagsAndDescription(q.id)).tags, { timeout: 8000 })
        .toEqual([]);
    } finally {
      await ctx.close();
      await api.deleteQuery(q.id).catch(() => {});
    }
  });
});

test("tag pill filters the queries list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const tag = `e2e-pill-${stamp}`;

  await withDataSource(api, async (dsId) => {
    const tagged = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-pill-tagged-${stamp}`,
      query: "SELECT 1",
    });
    const untagged = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-pill-untagged-${stamp}`,
      query: "SELECT 1",
    });
    await api.updateQuery(tagged.id, { tags: [tag] });

    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    try {
      await page.goto("/queries");
      await expect(page.getByText(tagged.name)).toBeVisible({ timeout: 8000 });
      await expect(page.getByText(untagged.name)).toBeVisible({ timeout: 8000 });

      const pill = page.getByRole("button", { name: tag, exact: true });
      await expect(pill).toBeVisible({ timeout: 8000 });
      await pill.click();

      await expect(page.getByText(tagged.name)).toBeVisible();
      await expect(page.getByText(untagged.name)).not.toBeVisible();

      await page.getByRole("button", { name: "clear" }).click();
      await expect(page.getByText(untagged.name)).toBeVisible({ timeout: 8000 });
    } finally {
      await ctx.close();
      await api.deleteQuery(tagged.id).catch(() => {});
      await api.deleteQuery(untagged.id).catch(() => {});
    }
  });
});

test("tag input autocompletes from org-wide existing tags", async ({ browser }) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const tag = `e2e-autocomplete-${stamp}`;

  await withDataSource(api, async (dsId) => {
    const seeded = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-autocomplete-seed-${stamp}`,
      query: "SELECT 1",
      tags: [tag],
    });
    const target = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-autocomplete-target-${stamp}`,
      query: "SELECT 1",
    });

    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    try {
      await page.goto(`/queries/${target.id}`);
      const tagInput = page.getByPlaceholder("add tag…");
      await expect(tagInput).toBeVisible({ timeout: 8000 });
      await tagInput.click();

      // Datalist options are in the DOM; confirms the org tag is offered.
      await expect(
        page.locator(`#query-tag-suggestions option[value="${tag}"]`),
      ).toHaveCount(1, { timeout: 8000 });
    } finally {
      await ctx.close();
      await api.deleteQuery(seeded.id).catch(() => {});
      await api.deleteQuery(target.id).catch(() => {});
    }
  });
});

test("description autosaves and shows truncated with a tooltip on the list", async ({ browser }) => {
  const api = await loginAsAdmin();
  const stamp = Date.now();
  const longDescription =
    "This is a deliberately long description used to verify truncation and tooltip behavior on the queries list page. ".repeat(3);

  await withDataSource(api, async (dsId) => {
    const q = await api.createQuery({
      data_source_id: dsId,
      name: `e2e-description-${stamp}`,
      query: "SELECT 1",
    });

    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    try {
      await page.goto(`/queries/${q.id}`);
      const descriptionInput = page.getByPlaceholder("Add a description…");
      await expect(descriptionInput).toBeVisible({ timeout: 8000 });
      await descriptionInput.fill(longDescription);

      await expect(page.getByText("Saved", { exact: true })).toBeVisible({ timeout: 8000 });

      await expect
        .poll(async () => (await queryTagsAndDescription(q.id)).description, { timeout: 8000 })
        .toBe(longDescription);

      await page.goto("/queries");
      // truncate never shortens the DOM text; assert the real clip (scrollWidth > clientWidth).
      const trigger = page.getByText(longDescription, { exact: true });
      await expect(trigger).toBeVisible({ timeout: 8000 });
      const isTruncated = await trigger.evaluate((el) => el.scrollWidth > el.clientWidth);
      expect(isTruncated).toBe(true);

      await trigger.hover();
      await expect(
        page.locator('[data-slot="tooltip-content"]').getByText(longDescription, { exact: true }),
      ).toBeVisible({ timeout: 8000 });
    } finally {
      await ctx.close();
      await api.deleteQuery(q.id).catch(() => {});
    }
  });
});
