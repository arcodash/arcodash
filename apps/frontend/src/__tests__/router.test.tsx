import { describe, it, expect, vi, beforeAll } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";

vi.mock("@/extensions", () => ({
  extensions: [
    {
      navItems: [],
      routes: [{ path: "/test-extension-route", element: <div>extension page</div> }],
    },
  ],
}));

const BASE = "http://localhost:8080";

// Cold-transforming the router and lazy chunks can take seconds; pay it once here.
beforeAll(async () => {
  await Promise.all([
    import("@/router"),
    import("@/pages/QueryEditorPage"),
    import("@/pages/DashboardPage"),
    import("@/pages/PublicDashboardPage"),
    import("@/pages/PublicEmbedPage"),
    import("@/pages/AiUsagePage"),
  ]);
}, 60_000);

/** Renders the real route tree at `path` (AuthProvider + router, memory history) so tests can start
 * at any URL. */
async function renderAppAt(path: string) {
  const { router } = await import("@/router");
  const memoryRouter = createMemoryRouter(router.routes, { initialEntries: [path] });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={memoryRouter} />
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("catch-all 404", () => {
  it("keeps the app shell (sidebar nav) for a signed-in user on a bad URL", async () => {
    await renderAppAt("/settings/access");

    expect(await screen.findByText("404")).toBeInTheDocument();
    expect(await screen.findByRole("navigation")).toBeInTheDocument();
    expect(screen.getByRole("banner")).toBeInTheDocument();
  });

  it("renders no app shell for a signed-out visitor on a bad URL", async () => {
    server.use(http.get(`${BASE}/api/session`, () => HttpResponse.json({ user: null })));
    await renderAppAt("/settings/access");

    expect(await screen.findByText("404")).toBeInTheDocument();
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(screen.queryByRole("banner")).not.toBeInTheDocument();
  });
});

describe("per-route document titles", () => {
  it("titles the Monitors list route", async () => {
    await renderAppAt("/monitors");
    await screen.findByRole("navigation");
    expect(document.title).toBe("Monitors · arcodash");
  });

  it("titles the Dashboards list route, distinctly from Monitors", async () => {
    await renderAppAt("/dashboards");
    await screen.findByRole("navigation");
    expect(document.title).toBe("Dashboards · arcodash");
  });

  it("titles the 404 page distinctly from every real route", async () => {
    await renderAppAt("/this-route-does-not-exist");
    const heading = await screen.findByText("404");
    expect(document.title).toBe("Page not found · arcodash");
    expect(heading.className).toContain("text-2xl");
    expect(heading.className).toContain("font-bold");
  });
});

describe("dynamic page titles are not overwritten by the route's static title", () => {
  it("keeps a monitor's own name in the title even when its query data is already cached on mount", async () => {
    // A parent route wrapper's static title must not clobber the page's own. With the object
    // already cached both effects run on first render and the parent's (later) wins; seeding the
    // cache reproduces that ordering.
    const monitor = {
      id: 42,
      name: "Too many signups",
      query: { id: 7, name: "Signups query" },
      options: { column: "count", op: ">", value: 100, muted: false },
      state: "ok",
      last_triggered_at: null,
      rearm: null,
      user: { id: 1, name: "Test User", email: "test@example.com" },
      org_id: 1,
      created_at: "2025-01-01T00:00:00Z",
      updated_at: "2025-01-01T00:00:00Z",
      caller: { object_role: "owner", can_edit: true, can_delete: true },
    };
    server.use(
      http.get(`${BASE}/api/monitors/42`, () => HttpResponse.json(monitor)),
      http.get(`${BASE}/api/monitors/42/subscriptions`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/monitors/42/deliveries`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/7/results`, () => new HttpResponse(null, { status: 404 })),
    );

    const { router } = await import("@/router");
    const memoryRouter = createMemoryRouter(router.routes, { initialEntries: ["/monitors/42"] });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["monitor", "42"], monitor);

    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <RouterProvider router={memoryRouter} />
        </AuthProvider>
      </QueryClientProvider>,
    );

    await screen.findByDisplayValue("Too many signups");
    expect(document.title).toBe("Too many signups · arcodash");
  });

  it("keeps a query's own name in the title on the query-editor route — queries/:id has no RouteTitle wrapper racing QueryEditorPage's own effect", async () => {
    const query = {
      id: 7,
      name: "Revenue by Category",
      query: "SELECT 1",
      data_source_id: 1,
      options: { parameters: [] },
      is_archived: false,
      published: false,
      has_unpublished_changes: false,
      working_version: 1,
      visibility: "workspace",
      is_favorite: false,
      version: 1,
      description: "",
      user_id: 1,
      owner_name: "Ada Lovelace",
      latest_query_data_id: null,
      last_execution_status: "New",
      last_execution_at: null,
      created_at: "2025-01-01T00:00:00Z",
      updated_at: "2025-01-01T00:00:00Z",
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
    };
    server.use(http.get(`${BASE}/api/queries/7`, () => HttpResponse.json(query)));

    await renderAppAt("/queries/7");

    // A stray RouteTitle here would pin the title to its static value, since QueryEditorPage never
    // re-runs its effect.
    await waitFor(() => expect(document.title).toBe("Revenue by Category · arcodash"));
  });
});

describe("router", () => {
  it("registers routes contributed by extensions", async () => {
    const { router } = await import("@/router");
    const match = router.routes.find((r) => r.path === "/test-extension-route");
    expect(match).toBeDefined();
  });

  it("lets an extension route override a fixed OSS path", async () => {
    vi.doMock("@/extensions", () => ({
      extensions: [
        {
          navItems: [],
          routes: [{ path: "/users/invitation/accept", element: <div>extension invite page</div> }],
        },
      ],
    }));
    vi.resetModules();
    const { router } = await import("@/router");
    const matches = router.routes.filter((r) => r.path === "/users/invitation/accept");
    expect(matches).toHaveLength(2); // both present in the array
    expect(matches[0].element).toEqual(<div>extension invite page</div>); // but the extension's is first
  });
});

describe("extension layoutRoutes", () => {
  it("splices layoutRoutes as children of the app-shell layout route, not as top-level routes", async () => {
    vi.doMock("@/extensions", () => ({
      extensions: [
        {
          navItems: [],
          routes: [],
          layoutRoutes: [{ path: "test-layout-route", element: <div>layout route page</div> }],
        },
      ],
    }));
    vi.resetModules();
    const { router } = await import("@/router");

    expect(router.routes.some((r) => r.path === "test-layout-route")).toBe(false);

    const shell = router.routes.find((r) => r.path === "/");
    const match = shell?.children?.find((r) => r.path === "test-layout-route");
    expect(match).toBeDefined();
    expect((match?.errorElement as { type?: { name?: string } } | undefined)?.type?.name).toBe(
      "RouteError",
    );
  });

  it("renders a layoutRoute page inside the one shared app shell, not a second one", async () => {
    vi.doMock("@/extensions", () => ({
      extensions: [
        {
          navItems: [],
          routes: [],
          layoutRoutes: [{ path: "test-layout-route", element: <div>layout route page</div> }],
        },
      ],
    }));
    vi.resetModules();
    // After vi.resetModules(), "@/router" and "@/lib/auth" must both come from this fresh import,
    // or AuthProvider/useAuth are different React contexts.
    const { router } = await import("@/router");
    const { AuthProvider } = await import("@/lib/auth");
    const memoryRouter = createMemoryRouter(router.routes, { initialEntries: ["/test-layout-route"] });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <RouterProvider router={memoryRouter} />
        </AuthProvider>
      </QueryClientProvider>,
    );

    expect(await screen.findByText("layout route page")).toBeInTheDocument();
    expect(await screen.findByRole("navigation")).toBeInTheDocument();
  });
});

describe("route precedence: /users/invitation/accept", () => {
  function ExtInviteAcceptPageStub() {
    return <div>extension invite accept</div>;
  }

  it("resolves to the extension's page when one registers the path (array order, first match wins)", async () => {
    vi.doMock("@/extensions", () => ({
      extensions: [
        { navItems: [], routes: [{ path: "/users/invitation/accept", element: <ExtInviteAcceptPageStub /> }] },
      ],
    }));
    vi.resetModules();
    const { router } = await import("@/router");

    const matches = router.routes.filter((r) => r.path === "/users/invitation/accept");
    expect(matches).toHaveLength(2); // the extension's entry plus the fixed OSS one, both present
    expect(matches[0].element).toEqual(<ExtInviteAcceptPageStub />);

    const winner = router.routes.find((r) => r.path === "/users/invitation/accept");
    expect(winner?.element).toEqual(<ExtInviteAcceptPageStub />);
  });

  it("falls back to the OSS InviteAcceptPage with no extension registering the path", async () => {
    vi.doMock("@/extensions", () => ({ extensions: [] }));
    vi.resetModules();
    const { router } = await import("@/router");
    const { InviteAcceptPage } = await import("@/pages/InviteAcceptPage");

    const matches = router.routes.filter((r) => r.path === "/users/invitation/accept");
    expect(matches).toHaveLength(1);
    expect(matches[0].element).toEqual(<InviteAcceptPage />);
  });
});

describe("route errorElement", () => {
  const PATHS = [
    "/public/dashboards/:token",
    "/public/embed/:token",
    "/",
    "/login",
    "/forgot_password",
    "/reset_password",
    "/users/invitation/accept",
  ];

  it.each(PATHS)("wires the shared RouteError element onto %s", async (path) => {
    const { router } = await import("@/router");
    const route = router.routes.find((r) => r.path === path);
    expect((route?.errorElement as { type?: { name?: string } } | undefined)?.type?.name).toBe(
      "RouteError",
    );
  });

  it.each(PATHS)(
    "renders a generic message and a Reload control for %s, with no error detail in the document",
    async (path) => {
      const { router } = await import("@/router");
      const errorElement = router.routes.find((r) => r.path === path)?.errorElement;

      function Thrower(): never {
        throw new Error("Cannot read properties of undefined (reading 'chunk-abc123')");
      }

      const memoryRouter = createMemoryRouter(
        [{ path: "/t", element: <Thrower />, errorElement }],
        { initialEntries: ["/t"] },
      );
      render(<RouterProvider router={memoryRouter} />);

      expect(await screen.findByText("Something went wrong loading this page.")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Reload" })).toBeInTheDocument();
      expect(document.body.textContent).not.toContain("chunk-abc123");
    },
  );
});
