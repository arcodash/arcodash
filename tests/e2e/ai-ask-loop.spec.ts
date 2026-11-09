/** E2E: "Ask with AI" streaming loop and "Fix with AI". The AI hop (POST /api/queries/ask) is faked
 * with canned SSE bodies; the execution error that triggers "Fix with AI" is real. */
import { test, expect } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { type E2EApiClient, loginAsAdmin, TARGET_DS_OPTIONS } from "./helpers/api";
import { sse } from "./helpers/ai-stub";

const QUESTION = "customers with no orders in the last 30 days";
const GOOD_SQL = "SELECT id, name FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)";

const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-cache" };

const preview = (attempt: number) =>
  [
    "preview",
    {
      attempt,
      columns: [
        { name: "id", friendly_type: "number" },
        { name: "name", friendly_type: "text" },
      ],
      rows: [
        [1, "Zelda Quux"],
        [2, "Ignatius Plonk"],
        [3, "Marguerite Vbar"],
      ],
      row_count: 3,
      truncated: false,
    },
  ] as [string, Record<string, unknown>];

const final = (
  sql: string,
  attempts: number,
  over: Record<string, unknown> = {},
): [string, Record<string, unknown>] => [
  "final",
  {
    sql,
    explanation: "Customers without orders.",
    served_by: "Gemini",
    attempts,
    tokens_in: 10,
    tokens_out: 5,
    executed: true,
    error: null,
    ...over,
  },
];

async function setup(browser: import("@playwright/test").Browser) {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-ask-loop-${Date.now()}-${Math.floor(Math.random() * 1e6)}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  await page.route("**/api/session", async (route) => {
    const res = await route.fetch();
    const json = await res.json();
    json.client_config = { ...(json.client_config ?? {}), ai_sql_generation: true };
    await route.fulfill({ json });
  });
  return { api, ds, ctx, page };
}

async function teardown(api: E2EApiClient, dsId: number, ctx: { close(): Promise<void> }) {
  await api.deleteDataSource(dsId).catch(() => {});
  await ctx.close();
}

test("happy path: stages, SQL, preview grid, collapsed summary, Apply", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    await page.route("**/api/queries/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["draft", { attempt: 1, sql: GOOD_SQL, explanation: null }],
          ["executing", { attempt: 1 }],
          preview(1),
          final(GOOD_SQL, 1),
        ]),
      }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    const timeline = page.getByTestId("ask-timeline");
    await expect(timeline).toHaveAttribute("data-phase", "done", { timeout: 10_000 });
    await expect(page.getByRole("button", { name: "Solved in 1 attempt · 3 rows · Gemini" })).toBeVisible();

    const apply = page.getByRole("button", { name: "Apply", exact: true });

    await expect(apply).toBeInViewport();
    await expect(page.getByRole("button", { name: "Regenerate" })).toBeInViewport();
    await expect(page.getByRole("button", { name: "Discard", exact: true })).toBeInViewport();
    expect(await page.evaluate(() => window.scrollY)).toBe(0);

    await timeline.getByRole("button", { name: /^Solved in/ }).click();
    await expect(timeline.locator("li", { hasText: "Generating SQL" })).toBeVisible();
    await expect(timeline.locator("li", { hasText: "Running preview" })).toBeVisible();
    await expect(timeline.locator("li", { hasText: "Preview:" })).toBeVisible();

    await expect(page.locator(".cm-content").filter({ hasText: "NOT IN (SELECT customer_id" })).toBeVisible();
    const grid = page.getByTestId("ask-preview-grid");
    await expect(grid).toContainText("Zelda Quux");
    await expect(grid).toContainText("Marguerite Vbar");

    await apply.click();
    await page.getByRole("button", { name: "SQL Editor" }).click();
    await expect(page.locator(".cm-content").filter({ hasText: "NOT IN (SELECT customer_id" })).toBeVisible();
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

const TALL_SQL = Array.from({ length: 15 }, (_, i) => `SELECT ${i + 1} AS c${i + 1}${i < 14 ? "," : ""}`).join("\n");
const manyRows = (n: number) => {
  const [name, data] = preview(1);
  return [name, { ...data, rows: Array.from({ length: n }, (_, i) => [i + 1, `Customer ${i + 1}`]), row_count: n }] as [
    string,
    Record<string, unknown>,
  ];
};

for (const height of [720, 900, 1080]) {
  test(`finished run fits a 1280x${height} viewport without scrolling`, async ({ browser }) => {
    const { api, ds, ctx, page } = await setup(browser);
    try {
      await page.setViewportSize({ width: 1280, height });
      await page.route("**/api/queries/ask", (route) =>
        route.fulfill({
          status: 200,
          headers: sseHeaders,
          body: sse([
            ["draft", { attempt: 1, sql: TALL_SQL, explanation: null }],
            manyRows(5),
            final(TALL_SQL, 1),
          ]),
        }),
      );
      await page.goto("/queries/new");
      await page.getByRole("button", { name: "✨ Ask with AI" }).click();
      await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
      await page.getByRole("button", { name: "Generate SQL" }).click();
      await expect(page.getByTestId("ask-timeline")).toHaveAttribute("data-phase", "done", { timeout: 10_000 });

      // No clicks before these checks: clicks auto-scroll and would hide a layout overflow.
      const discard = page.getByRole("button", { name: "Discard", exact: true });
      for (const loc of [
        page.getByTestId("ask-sql-preview"),
        page.getByTestId("ask-preview-grid"),
        page.getByRole("button", { name: "Apply", exact: true }),
        page.getByRole("button", { name: "Regenerate" }),
        discard,
      ]) {
        await expect(loc).toBeInViewport({ ratio: 1 });
      }
      expect(await page.evaluate(() => document.scrollingElement!.scrollTop)).toBe(0);
      const scrolled = await discard.evaluate((el) => {
        const out: number[] = [];
        for (let n = el.parentElement; n; n = n.parentElement) {
          if (/auto|scroll/.test(getComputedStyle(n).overflowY)) out.push(n.scrollTop);
        }
        return out;
      });
      expect(scrolled.every((t) => t === 0)).toBe(true);
    } finally {
      await teardown(api, ds.id, ctx);
    }
  });
}

test("narrow viewport keeps the fixed caps and the page scrolls", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    await page.setViewportSize({ width: 500, height: 720 });
    await page.route("**/api/queries/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([["draft", { attempt: 1, sql: TALL_SQL, explanation: null }], manyRows(5), final(TALL_SQL, 1)]),
      }),
    );
    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();
    await expect(page.getByTestId("ask-timeline")).toHaveAttribute("data-phase", "done", { timeout: 10_000 });
    const avail = await page.getByTestId("ask-sql-preview").evaluate((el) => {
      let n: HTMLElement | null = el as HTMLElement;
      while (n && !n.style.getPropertyValue("--ask-avail")) n = n.parentElement;
      return n ? n.style.getPropertyValue("--ask-avail") : "";
    });
    expect(avail).toBe("");
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toBeVisible();
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("repair path: error stage, only latest draft in editor, 2 attempts", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  const badSql = "SELECT nope_col FROM customers";
  try {
    await page.route("**/api/queries/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["draft", { attempt: 1, sql: badSql, explanation: null }],
          ["executing", { attempt: 1 }],
          ["repair", { attempt: 1, error: 'column "nope_col" does not exist' }],
          ["draft", { attempt: 2, sql: GOOD_SQL, explanation: null }],
          ["executing", { attempt: 2 }],
          preview(2),
          final(GOOD_SQL, 2),
        ]),
      }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    const timeline = page.getByTestId("ask-timeline");
    await expect(page.getByRole("button", { name: "Solved in 2 attempts · 3 rows · Gemini" })).toBeVisible({
      timeout: 10_000,
    });
    await timeline.getByRole("button", { name: /^Solved in/ }).click();

    const repair = timeline.locator("li", { hasText: "→ repairing" });
    await expect(repair).toHaveText(/Error:\s*column "nope_col" does not exist\s*→ repairing/);
    const draft1 = timeline.getByRole("button", { name: "Draft 1" });
    await expect(draft1).toHaveAttribute("aria-expanded", "false");
    await expect(timeline.getByRole("button", { name: "Draft 2" })).toBeVisible();

    // Only the latest draft is shown in CodeMirror; the bad one stays in the collapsed entry.
    await expect(page.locator(".cm-content").filter({ hasText: "NOT IN (SELECT customer_id" })).toBeVisible();
    await expect(page.locator(".cm-content").filter({ hasText: "nope_col" })).toHaveCount(0);
    await draft1.click();
    await expect(timeline.locator("pre", { hasText: badSql })).toBeVisible();
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("zero-row preview is a success", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  try {
    await page.route("**/api/queries/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["draft", { attempt: 1, sql: GOOD_SQL, explanation: null }],
          ["executing", { attempt: 1 }],
          [
            "preview",
            {
              attempt: 1,
              columns: [
                { name: "id", friendly_type: "number" },
                { name: "name", friendly_type: "text" },
              ],
              rows: [],
              row_count: 0,
              truncated: false,
            },
          ],
          final(GOOD_SQL, 1),
        ]),
      }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    await expect(page.getByRole("button", { name: /Solved in 1 attempt · 0 rows/ })).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByTestId("ask-preview-grid")).toContainText("Query ran successfully — no rows");
    await expect(page.getByRole("alert").filter({ hasText: "This SQL didn't run successfully" })).toHaveCount(0);
    const apply = page.getByRole("button", { name: "Apply", exact: true });
    await expect(apply).toBeEnabled();
    await apply.click();
    await page.getByRole("button", { name: "SQL Editor" }).click();
    await expect(page.locator(".cm-content").filter({ hasText: "NOT IN (SELECT customer_id" })).toBeVisible();
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("exhausted run warns but keeps Apply; failed/not_configured shows the error", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  const badSql = "SELECT nope_col FROM customers";
  try {
    let mode: "exhausted" | "failed" = "exhausted";
    await page.route("**/api/queries/ask", (route) =>
      route.fulfill({
        status: 200,
        headers: sseHeaders,
        body:
          mode === "exhausted"
            ? sse([
                ["status", { text: "Generating SQL" }],
                ["draft", { attempt: 1, sql: badSql, explanation: null }],
                ["executing", { attempt: 1 }],
                ["repair", { attempt: 1, error: 'column "nope_col" does not exist' }],
                final(badSql, 1, { executed: false, error: 'column "nope_col" does not exist' }),
              ])
            : sse([["failed", { status: "not_configured", message: "AI is not configured." }]]),
      }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    await expect(page.getByRole("button", { name: /^Didn't run successfully after 1 attempt/ })).toBeVisible({
      timeout: 10_000,
    });
    const warning = page.getByRole("alert").filter({ hasText: "This SQL didn't run successfully" });
    await expect(warning).toContainText("nope_col");
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toBeEnabled();

    await page.getByRole("button", { name: "Discard" }).click();
    mode = "failed";
    await page.getByRole("button", { name: "Generate SQL" }).click();
    const timeline = page.getByTestId("ask-timeline");
    await expect(timeline).toHaveAttribute("data-phase", "failed", { timeout: 10_000 });
    await expect(timeline.getByRole("alert")).toContainText(/not configured/i);
    await expect(timeline.locator("li", { hasText: "Generating SQL" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toHaveCount(0);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("cancel aborts a hanging stream; Discard only appears afterwards; nothing applied", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  let release!: () => void;
  const held = new Promise<void>((r) => (release = r));
  try {
    await page.route("**/api/queries/ask", async (route) => {
      await held;
      await route.abort().catch(() => {});
    });

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    const timeline = page.getByTestId("ask-timeline");
    await expect(timeline).toHaveAttribute("data-phase", "running");
    await expect(page.getByRole("button", { name: "Discard" })).toHaveCount(0);
    await page.getByRole("button", { name: "Cancel" }).click();

    await expect(timeline).toHaveAttribute("data-phase", "cancelled");
    await expect(page.getByRole("button", { name: "Cancel" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Apply", exact: true })).toHaveCount(0);
    await page.getByRole("button", { name: "SQL Editor" }).click();
    await expect(page.locator(".cm-content").filter({ hasText: "NOT IN" })).toHaveCount(0);
  } finally {
    release();
    await teardown(api, ds.id, ctx);
  }
});

test("Fix with AI: real execution error → Ask tab prefilled → Apply puts fixed SQL in editor", async ({
  browser,
}) => {
  const { api, ds, ctx, page } = await setup(browser);
  const q = await api.createQuery({
    data_source_id: ds.id,
    name: `ask-fix-${Date.now()}`,
    query: "SELECT no_such_col_e2e FROM customers",
  });
  try {
    let body: Record<string, unknown> | null = null;
    await page.route("**/api/queries/ask", (route) => {
      body = route.request().postDataJSON();
      return route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["status", { text: "Generating SQL" }],
          ["draft", { attempt: 1, sql: "SELECT id FROM customers", explanation: null }],
          ["repair", { attempt: 1, error: "still wrong" }],
          ["draft", { attempt: 2, sql: "SELECT name FROM customers", explanation: null }],
          ["executing", { attempt: 2 }],
          preview(2),
          final("SELECT name FROM customers", 2),
        ]),
      });
    });

    await page.goto(`/queries/${q.id}`);
    await page.getByRole("button", { name: /execute/i }).click();
    await expect(page.getByText(/no_such_col_e2e/i).first()).toBeVisible({ timeout: 20_000 });

    await page.getByRole("status").getByRole("button", { name: "✨ Fix with AI" }).click({ timeout: 5000 });
    await expect(page.getByLabel("Ask AI a question about your data")).toHaveValue(/^Fix: .*no_such_col_e2e/);
    await expect(page.getByRole("button", { name: /^Solved in 2 attempts/ })).toBeVisible({ timeout: 10_000 });

    expect(body).not.toBeNull();
    expect(body!.existing_sql).toBe("SELECT no_such_col_e2e FROM customers");
    expect(String(body!.error)).toContain("no_such_col_e2e");

    await page.getByRole("button", { name: "Apply", exact: true }).click();
    await page.getByRole("button", { name: "SQL Editor" }).click();
    await expect(page.locator(".cm-content").filter({ hasText: "SELECT name FROM customers" })).toBeVisible();
  } finally {
    await api.deleteQuery(q.id).catch(() => {});
    await teardown(api, ds.id, ctx);
  }
});

test("profile_refreshing: waits for the schema, asks again exactly once, shows the result", async ({
  browser,
}) => {
  const { api, ds, ctx, page } = await setup(browser);
  const badSql = "SELECT old_col FROM customers";
  try {
    const bodies: unknown[] = [];
    await page.route("**/api/queries/ask", (route) => {
      bodies.push(route.request().postDataJSON());
      return route.fulfill({
        status: 200,
        headers: sseHeaders,
        body:
          bodies.length === 1
            ? sse([
                ["profile_refreshing", { data_source_id: ds.id }],
                final(badSql, 0, { executed: false, error: 'column "old_col" does not exist' }),
              ])
            : sse([
                ["draft", { attempt: 1, sql: GOOD_SQL, explanation: null }],
                ["executing", { attempt: 1 }],
                preview(1),
                final(GOOD_SQL, 1),
              ]),
      });
    });
    // Any data source: the page may default to one left over from another spec.
    // Refreshing until the test has seen the waiting state, so it can't finish before the assert.
    let refreshing = true;
    await page.route("**/api/data_sources/*/schema*", (route) =>
      route.fulfill({ json: { schema: { tables: [] }, refreshing, refresh_error: null } }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    const timeline = page.getByTestId("ask-timeline");
    await expect(page.getByTestId("ask-step-refreshing")).toBeVisible({ timeout: 10_000 });
    await expect(timeline).toContainText("Updating the schema…");
    expect(bodies).toHaveLength(1);
    refreshing = false;
    const summary = page.getByRole("button", { name: "Solved in 1 attempt · 3 rows · Gemini" });
    await expect(summary).toBeVisible({ timeout: 15_000 });
    await summary.click();
    await expect(page.getByTestId("ask-step-retrying")).toBeVisible();
    expect(bodies).toHaveLength(2);
    expect(bodies[1]).toEqual(bodies[0]);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});

test("profile_refreshing again on the retry: normal failure, no third request", async ({ browser }) => {
  const { api, ds, ctx, page } = await setup(browser);
  const badSql = "SELECT old_col FROM customers";
  try {
    let asks = 0;
    await page.route("**/api/queries/ask", (route) => {
      asks++;
      return route.fulfill({
        status: 200,
        headers: sseHeaders,
        body: sse([
          ["profile_refreshing", { data_source_id: ds.id }],
          final(badSql, 0, { executed: false, error: 'column "old_col" does not exist' }),
        ]),
      });
    });
    // Any data source: the page may default to one left over from another spec.
    await page.route("**/api/data_sources/*/schema*", (route) =>
      route.fulfill({ json: { schema: { tables: [] }, refreshing: false, refresh_error: null } }),
    );

    await page.goto("/queries/new");
    await page.getByRole("button", { name: "✨ Ask with AI" }).click();
    await page.getByLabel("Ask AI a question about your data").fill(QUESTION);
    await page.getByRole("button", { name: "Generate SQL" }).click();

    const summary = page.getByRole("button", { name: /^Didn't run successfully after 0 attempts/ });
    await expect(summary).toBeVisible({ timeout: 10_000 });
    await summary.click();
    await expect(page.getByTestId("ask-step-retrying")).toBeVisible();
    expect(asks).toBe(2);
  } finally {
    await teardown(api, ds.id, ctx);
  }
});
