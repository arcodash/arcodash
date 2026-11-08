// Requires: backend :8080, frontend :3000, target postgres :5434.
import { test, expect, type Page } from "@playwright/test";
import { adminContext } from "./helpers/auth";
import { loginAsAdmin, type E2EApiClient, TARGET_DS_OPTIONS } from "./helpers/api";

// ActionBar collapses by container width: 1920/1440/1280 render inline buttons, 1024/768 fall back
// to "More actions".
const WIDTHS = [1920, 1440, 1280, 1024, 768];

test("saved query editor header has no horizontal overflow across widths", async ({ browser }) => {
  const api = await loginAsAdmin();
  const ds = await api.createDataSource({
    name: `e2e-responsive-${Date.now()}`,
    type: "pg",
    options: TARGET_DS_OPTIONS,
  });
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();

  try {
    const q = await api.createQuery({
      data_source_id: ds.id,
      name: `responsive-test-${Date.now()}`,
      query: "SELECT 1",
    });
    try {
      await page.goto(`/queries/${q.id}`);
      await expect(page.locator("#query-name")).toBeVisible({ timeout: 8000 });
      // Wide-row buttons render only after the saved query loads and are CSS-hidden at 1280x720, so
      // wait for [data-action] attachment, not visibility.
      await page.locator('[data-action="fork"]').waitFor({ state: "attached", timeout: 8000 });

      for (const width of WIDTHS) {
        await page.setViewportSize({ width, height: 900 });

        const overflow = await page.evaluate(() => ({
          scrollWidth: document.documentElement.scrollWidth,
          clientWidth: document.documentElement.clientWidth,
        }));
        expect(
          overflow.scrollWidth,
          `viewport ${width}: scrollWidth ${overflow.scrollWidth} > clientWidth ${overflow.clientWidth}`,
        ).toBeLessThanOrEqual(overflow.clientWidth);

        // Execute is the one primary action always on screen.
        const executeBtn = page.locator('button:has-text("Execute")');
        await expect(executeBtn).toBeVisible();
        const executeBox = await executeBtn.boundingBox();
        expect(executeBox).not.toBeNull();
        expect(executeBox!.x).toBeGreaterThanOrEqual(0);
        expect(executeBox!.x + executeBox!.width).toBeLessThanOrEqual(width);
      }
    } finally {
      await api.deleteQuery(q.id).catch(() => {});
    }
  } finally {
    await ctx.close();
    await api.deleteDataSource(ds.id).catch(() => {});
  }
});

const MATRIX_WIDTHS = [1920, 1440, 1280, 1024, 768, 390];

async function assertNoHorizontalOverflow(page: Page, width: number) {
  // DashboardGrid measures via ResizeObserver + rAF, so poll to avoid mistaking settling for
  // overflow.
  await expect(async () => {
    const overflow = await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      clientWidth: document.documentElement.clientWidth,
    }));
    expect(
      overflow.scrollWidth,
      `viewport ${width}: scrollWidth ${overflow.scrollWidth} > clientWidth ${overflow.clientWidth}`,
    ).toBeLessThanOrEqual(overflow.clientWidth);
  }).toPass({ timeout: 2000 });
}

interface RouteConfig {
  name: string;
  setup: (api: E2EApiClient) => Promise<{ path: string; cleanup: () => Promise<void> }>;
  perWidth: (page: Page, width: number) => Promise<void>;
}

const ROUTE_CONFIGS: RouteConfig[] = [
  {
    name: "queries list",
    setup: async () => ({ path: "/queries", cleanup: async () => {} }),
    perWidth: async (page, width) => {
      await expect(page.getByRole("link", { name: "New Query" })).toBeVisible();
      if (width < 768) {
        await expect(page.getByRole("button", { name: /^filter/i })).toBeVisible();
        await expect(page.getByRole("button", { name: "All owners", exact: true })).toBeHidden();
      } else {
        await expect(page.getByRole("button", { name: "All owners", exact: true })).toBeVisible();
        await expect(page.getByRole("button", { name: /^filter/i })).toBeHidden();
      }
    },
  },
  {
    name: "dashboards list",
    setup: async () => ({ path: "/dashboards", cleanup: async () => {} }),
    perWidth: async (page, width) => {
      await expect(page.getByRole("button", { name: "New Dashboard" })).toBeVisible();
      if (width < 768) {
        await expect(page.getByRole("button", { name: /^filter/i })).toBeVisible();
      } else {
        await expect(page.getByRole("button", { name: "All owners", exact: true })).toBeVisible();
      }
    },
  },
  {
    name: "dashboard detail",
    setup: async (api) => {
      const dash = await api.createDashboard(`e2e-responsive-dash-${Date.now()}`);
      for (let i = 0; i < 3; i++) {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: null,
          text: `Responsive widget ${i}`,
          options: { position: { col: i * 2, row: 0, sizeX: 2, sizeY: 2 } },
        });
      }
      // View mode shows the published snapshot; unpublished, the empty-state text collides with the
      // "Edit" button name.
      await api.publishDashboard(dash.slug);
      return {
        path: `/dashboard/${dash.slug}`,
        cleanup: async () => {
          await api.deleteDashboard(dash.slug).catch(() => {});
        },
      };
    },
    perWidth: async (page, width) => {
      await expect(page.getByRole("button", { name: "Edit", exact: true })).toBeVisible();
      if (width < 768) {
        await expect(page.getByTestId("dashboard-stack")).toBeVisible();
      } else {
        await expect(page.getByTestId("dashboard-stack")).toHaveCount(0);
      }
    },
  },
  {
    name: "data sources list",
    setup: async () => ({ path: "/data_sources", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: "+ New data source" })).toBeVisible();
    },
  },
  {
    name: "data source new",
    setup: async () => ({ path: "/data_sources/new", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: "Create" })).toBeVisible();
    },
  },
  {
    name: "monitors list",
    setup: async (api) => {
      const ds = await api.createDataSource({
        name: `e2e-responsive-monitors-ds-${Date.now()}`,
        type: "pg",
        options: TARGET_DS_OPTIONS,
      });
      const q = await api.createQuery({
        data_source_id: ds.id,
        name: `e2e-responsive-monitors-q-${Date.now()}`,
        query: "SELECT 1",
      });
      await api.publishQuery(q.id);
      const monitor = await api.createMonitor({
        query_id: q.id,
        name: `e2e-responsive-monitor-${Date.now()}`,
        options: { column: "count", op: ">", value: 0 },
      });
      return {
        path: "/monitors",
        cleanup: async () => {
          await api.deleteMonitor(monitor.id).catch(() => {});
          await api.deleteQuery(q.id).catch(() => {});
          await api.deleteDataSource(ds.id).catch(() => {});
        },
      };
    },
    perWidth: async (page, width) => {
      await expect(page.getByRole("button", { name: "+ New Monitor" })).toBeVisible();
      if (width < 768) {
        await expect(page.getByRole("button", { name: /^filter/i })).toBeVisible();
      } else {
        await expect(page.getByRole("button", { name: "All owners", exact: true })).toBeVisible();
      }
    },
  },
  {
    name: "monitor detail",
    setup: async (api) => {
      const ds = await api.createDataSource({
        name: `e2e-responsive-monitordet-ds-${Date.now()}`,
        type: "pg",
        options: TARGET_DS_OPTIONS,
      });
      const q = await api.createQuery({
        data_source_id: ds.id,
        name: `e2e-responsive-monitordet-q-${Date.now()}`,
        query: "SELECT 1",
      });
      await api.publishQuery(q.id);
      const monitor = await api.createMonitor({
        query_id: q.id,
        name: `e2e-responsive-monitordet-${Date.now()}`,
        options: { column: "count", op: ">", value: 0 },
      });
      return {
        path: `/monitors/${monitor.id}`,
        cleanup: async () => {
          await api.deleteMonitor(monitor.id).catch(() => {});
          await api.deleteQuery(q.id).catch(() => {});
          await api.deleteDataSource(ds.id).catch(() => {});
        },
      };
    },
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: /^save$/i })).toBeVisible();
    },
  },
  {
    name: "destinations list",
    setup: async (api) => {
      const dest = await api.createDestination({
        name: `e2e-responsive-dest-${Date.now()}`,
        type: "webhook",
        options: { url: "https://example.com/hook" },
      });
      return {
        path: "/destinations",
        cleanup: async () => {
          await api.deleteDestination(dest.id).catch(() => {});
        },
      };
    },
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: "+ New Destination" })).toBeVisible();
    },
  },
  {
    name: "users list",
    setup: async () => ({ path: "/users", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: "+ Invite User" })).toBeVisible();
    },
  },
  {
    name: "groups list",
    setup: async (api) => {
      const group = await api.createGroup({ name: `e2e-responsive-group-${Date.now()}` });
      return {
        path: "/access?tab=groups",
        cleanup: async () => {
          await api.deleteGroup(group.id).catch(() => {});
        },
      };
    },
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: /new group/i })).toBeVisible();
    },
  },
  {
    name: "organization settings",
    setup: async () => ({ path: "/organization", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("button", { name: /^save$/i })).toBeVisible();
    },
  },
  {
    name: "AI settings",
    setup: async () => ({ path: "/organization/ai", cleanup: async () => {} }),
    perWidth: async (page) => {
      // AiSettingsPage has a "Save" per ProviderCard; scope to one.
      await expect(
        page.getByTestId("provider-card-gemini").getByRole("button", { name: /^save$/i }),
      ).toBeVisible();
    },
  },
  {
    name: "system status",
    setup: async () => ({ path: "/admin/status", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("heading", { name: "System Status" })).toBeVisible();
    },
  },
  {
    name: "audit log",
    setup: async () => ({ path: "/admin/audit", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByPlaceholder("Filter by action…")).toBeVisible();
    },
  },
  {
    name: "settings hub",
    setup: async () => ({ path: "/settings", cleanup: async () => {} }),
    perWidth: async (page) => {
      await expect(page.getByRole("link", { name: /Users/ })).toBeVisible();
    },
  },
  {
    name: "public dashboard",
    setup: async (api) => {
      const dash = await api.createDashboard(`e2e-responsive-public-${Date.now()}`);
      for (let i = 0; i < 3; i++) {
        await api.createWidget({
          dashboard_id: dash.id,
          visualization_id: null,
          text: `Public responsive widget ${i}`,
          options: { position: { col: i * 2, row: 0, sizeX: 2, sizeY: 2 } },
        });
      }
      await api.publishDashboard(dash.slug); // share/embed creation requires a published dashboard
      const { secret_token } = await api.shareDashboard(dash.slug);
      return {
        path: `/public/dashboards/${secret_token}`,
        cleanup: async () => {
          await api.revokeShareDashboard(dash.slug).catch(() => {});
          await api.deleteDashboard(dash.slug).catch(() => {});
        },
      };
    },
    perWidth: async (page, width) => {
      if (width < 768) {
        await expect(page.getByTestId("dashboard-stack")).toBeVisible();
      } else {
        await expect(page.getByTestId("dashboard-stack")).toHaveCount(0);
      }
    },
  },
];

test.describe("Responsive matrix (all routes)", () => {
  for (const cfg of ROUTE_CONFIGS) {
    test(`${cfg.name} — no overflow, primary actions reachable, across widths`, async ({ browser }) => {
      const api = await loginAsAdmin();
      const { path, cleanup } = await cfg.setup(api);
      const ctx = await adminContext(browser);
      const page = await ctx.newPage();
      try {
        await page.goto(path);
        await page.waitForLoadState("networkidle");
        for (const width of MATRIX_WIDTHS) {
          await page.setViewportSize({ width, height: 900 });
          await assertNoHorizontalOverflow(page, width);
          await cfg.perWidth(page, width);
        }
      } finally {
        await ctx.close();
        await cleanup();
      }
    });
  }

  test("root/AppShell — hamburger visibility and drawer nav across widths", async ({ browser }) => {
    const ctx = await adminContext(browser);
    const page = await ctx.newPage();
    try {
      await page.goto("/");
      await page.waitForURL(/\/queries/);

      for (const width of MATRIX_WIDTHS) {
        await page.setViewportSize({ width, height: 900 });
        await assertNoHorizontalOverflow(page, width);
        const hamburger = page.getByRole("button", { name: "Open navigation" });
        if (width < 768) {
          await expect(hamburger).toBeVisible();
        } else {
          await expect(hamburger).toBeHidden();
        }
      }

      await page.setViewportSize({ width: 390, height: 900 });
      await page.getByRole("button", { name: "Open navigation" }).click();
      const dialog = page.getByRole("dialog");
      await expect(dialog).toBeVisible();
      await dialog.getByRole("link", { name: "Dashboards" }).click();
      await page.waitForURL(/\/dashboards/);
      await expect(page.getByRole("dialog")).toHaveCount(0);
    } finally {
      await ctx.close();
    }
  });

  // Unauthenticated context: adminContext would redirect /login away.
  const AUTH_PATHS = ["/login", "/forgot_password", "/reset_password", "/users/invitation/accept"];

  test("auth pages — no horizontal overflow across widths", async ({ browser }) => {
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    try {
      for (const path of AUTH_PATHS) {
        await page.goto(path);
        await page.waitForLoadState("networkidle");
        for (const width of MATRIX_WIDTHS) {
          await page.setViewportSize({ width, height: 900 });
          await assertNoHorizontalOverflow(page, width);
        }
      }
    } finally {
      await ctx.close();
    }
  });
});

// Every page's title starts at the same x (gutter comes from AppShell alone).
test("page content starts at the same x across pages with different column widths", async ({ browser }) => {
  const ctx = await adminContext(browser);
  const page = await ctx.newPage();
  try {
    await page.setViewportSize({ width: 1280, height: 900 });
    const routes = ["/queries", "/admin/audit", "/admin/status", "/organization", "/data_sources/new"];
    const xs: number[] = [];
    for (const route of routes) {
      await page.goto(route);
      const h1 = page.locator("h1").first();
      await expect(h1).toBeVisible();
      const box = await h1.boundingBox();
      expect(box, `${route}: h1 has no bounding box`).not.toBeNull();
      xs.push(box!.x);
    }
    for (let i = 1; i < xs.length; i++) {
      expect(xs[i], `${routes[i]} (x=${xs[i]}) vs ${routes[0]} (x=${xs[0]})`).toBeCloseTo(xs[0], 0);
    }
  } finally {
    await ctx.close();
  }
});

// jsdom can't measure overflow: the table's scrollWidth must not exceed its wrapper, with the
// sidebar expanded and collapsed.
const LIST_TABLE_WIDTHS = [1280, 1440];

async function assertListTableFits(page: Page) {
  const result = await page.evaluate(() => {
    const table = document.querySelector("table");
    if (!table || !table.parentElement) return null;
    return {
      tableScrollWidth: table.scrollWidth,
      wrapperClientWidth: table.parentElement.clientWidth,
      docScrollWidth: document.documentElement.scrollWidth,
      innerWidth: window.innerWidth,
    };
  });
  expect(result, "no <table> found on the page").not.toBeNull();
  expect(
    result!.tableScrollWidth,
    `table.scrollWidth ${result!.tableScrollWidth} > wrapper.clientWidth ${result!.wrapperClientWidth}`,
  ).toBeLessThanOrEqual(result!.wrapperClientWidth + 1);
  expect(
    result!.docScrollWidth,
    `document.scrollWidth ${result!.docScrollWidth} > innerWidth ${result!.innerWidth}`,
  ).toBeLessThanOrEqual(result!.innerWidth);
}

// The toggle is identified by its accessible name; the rail animates width, so wait for the class,
// then for the box to stop moving.
async function toggleSidebar(page: Page, target: "w-16" | "w-72") {
  await page.getByRole("button", { name: /^(Collapse sidebar|Expand sidebar)$/ }).click();
  const aside = page.locator("aside").first();
  await expect(aside).toHaveClass(new RegExp(`\\b${target}\\b`));
  // The class flips at transition start; wait for the box to stop moving.
  await aside.evaluate(
    (el) =>
      new Promise<void>((resolve) => {
        let last = el.getBoundingClientRect().width;
        let stableFrames = 0;
        function tick() {
          const width = el.getBoundingClientRect().width;
          if (width === last) {
            stableFrames++;
            if (stableFrames >= 3) {
              resolve();
              return;
            }
          } else {
            stableFrames = 0;
            last = width;
          }
          requestAnimationFrame(tick);
        }
        requestAnimationFrame(tick);
      }),
  );
}

interface ListTableRoute {
  name: string;
  setup: (api: E2EApiClient) => Promise<{ path: string; cleanup: () => Promise<void> }>;
}

const LIST_TABLE_ROUTES: ListTableRoute[] = [
  {
    name: "queries",
    setup: async (api) => {
      const ds = await api.createDataSource({
        name: `e2e-list-overflow-ds-${Date.now()}`,
        type: "pg",
        options: TARGET_DS_OPTIONS,
      });
      // A long name must truncate the Name column, not push Status/Actions off-screen.
      const q = await api.createQuery({
        data_source_id: ds.id,
        name: `A rather long query name meant to stress the flexible name column ${Date.now()}`,
        query: "SELECT 1",
      });
      return {
        path: "/queries",
        cleanup: async () => {
          await api.deleteQuery(q.id).catch(() => {});
          await api.deleteDataSource(ds.id).catch(() => {});
        },
      };
    },
  },
  {
    name: "dashboards",
    setup: async (api) => {
      const dash = await api.createDashboard(
        `A rather long dashboard name meant to stress the flexible name column ${Date.now()}`,
      );
      return {
        path: "/dashboards",
        cleanup: async () => {
          await api.deleteDashboard(dash.slug).catch(() => {});
        },
      };
    },
  },
  {
    name: "monitors",
    setup: async (api) => {
      const ds = await api.createDataSource({
        name: `e2e-list-overflow-mds-${Date.now()}`,
        type: "pg",
        options: TARGET_DS_OPTIONS,
      });
      const q = await api.createQuery({
        data_source_id: ds.id,
        name: `e2e-list-overflow-mq-${Date.now()}`,
        query: "SELECT 1",
      });
      await api.publishQuery(q.id);
      const monitor = await api.createMonitor({
        query_id: q.id,
        name: `A rather long monitor name meant to stress the flexible name column ${Date.now()}`,
        options: { column: "count", op: ">", value: 0 },
      });
      return {
        path: "/monitors",
        cleanup: async () => {
          await api.deleteMonitor(monitor.id).catch(() => {});
          await api.deleteQuery(q.id).catch(() => {});
          await api.deleteDataSource(ds.id).catch(() => {});
        },
      };
    },
  },
];

test.describe("list table fits its container at 1280/1440px", () => {
  for (const route of LIST_TABLE_ROUTES) {
    test(`${route.name} list — no table overflow, sidebar expanded and collapsed`, async ({ browser }) => {
      const api = await loginAsAdmin();
      const { path, cleanup } = await route.setup(api);
      const ctx = await adminContext(browser);
      const page = await ctx.newPage();
      try {
        for (const width of LIST_TABLE_WIDTHS) {
          await page.setViewportSize({ width, height: 900 });
          await page.goto(path);
          await page.waitForLoadState("networkidle");
          await assertListTableFits(page);

          await toggleSidebar(page, "w-16");
          await assertListTableFits(page);
          await toggleSidebar(page, "w-72"); // restore before the next width
        }
      } finally {
        await ctx.close();
        await cleanup();
      }
    });
  }
});
