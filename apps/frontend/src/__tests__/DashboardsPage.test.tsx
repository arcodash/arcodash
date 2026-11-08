import { describe, it, expect, vi, afterEach, beforeAll, afterAll } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { mockViewport } from "./mockViewport";
import { DashboardsPage } from "@/pages/DashboardsPage";

const BASE = "http://localhost:8080";

const DASHBOARDS = [
  {
    id: 1,
    name: "Growth Overview",
    slug: "growth-overview",
    user_id: 1,
    owner_name: "Test User",
    visibility: "restricted",
    published: true,
    has_unpublished_changes: false,
    is_favorite: false,
    tags: [],
    updated_at: "2025-01-01T00:00:00Z",
    caller: { object_role: "owner", can_edit: true, can_delete: true },
  },
];

// setup.ts stubs matchMedia false (mobile cards view); pin desktop so tests see the table.
let restoreViewport: () => void;
beforeAll(() => {
  restoreViewport = mockViewport(true);
});
afterAll(() => {
  restoreViewport();
});

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <DashboardsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DashboardsPage search debounce", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps the grid rendered while typing instead of flashing the full-page loader", async () => {
    const seenSearchParams: (string | null)[] = [];
    server.use(
      http.get(`${BASE}/api/dashboards`, ({ request }) => {
        const url = new URL(request.url);
        seenSearchParams.push(url.searchParams.get("q"));
        return HttpResponse.json(DASHBOARDS);
      }),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
    );

    renderPage();
    expect(await screen.findByText("Growth Overview")).toBeInTheDocument();

    vi.useFakeTimers();
    const input = screen.getByPlaceholderText("Search dashboards…");

    fireEvent.change(input, { target: { value: "g" } });
    fireEvent.change(input, { target: { value: "gr" } });
    fireEvent.change(input, { target: { value: "gro" } });

    expect(screen.getByText("Growth Overview")).toBeInTheDocument();
    expect(screen.queryByText(/loading your data/i)).not.toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(300);
    });
    vi.useRealTimers();

    await waitFor(() => {
      expect(seenSearchParams).toContain("gro");
    });
    // One debounced request for the keystroke burst (the initial fetch has no "q").
    expect(seenSearchParams.filter((p) => p !== null).length).toBe(1);
  });
});

describe("DashboardsPage owner/visibility chips + scope", () => {
  it("shows the owner and visibility chip on each card", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards`, () => HttpResponse.json(DASHBOARDS)),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
      http.get(`${BASE}/api/users`, () =>
        HttpResponse.json([{ id: 1, name: "Test User", email: "test@example.com" }]),
      ),
    );

    renderPage();
    expect(await screen.findByText("Growth Overview")).toBeInTheDocument();
    expect(screen.getByText("Test User")).toBeInTheDocument();
    expect(screen.getByText("Published")).toBeInTheDocument();
  });

  it("wires the scope toggle to the ?filter= param", async () => {
    const seenFilters: (string | null)[] = [];
    server.use(
      http.get(`${BASE}/api/dashboards`, ({ request }) => {
        seenFilters.push(new URL(request.url).searchParams.get("filter"));
        return HttpResponse.json(DASHBOARDS);
      }),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );

    renderPage();
    expect(await screen.findByText("Growth Overview")).toBeInTheDocument();
    expect(seenFilters).toContain(null);

    fireEvent.click(screen.getByRole("button", { name: "Shared with me" }));
    await waitFor(() => expect(seenFilters).toContain("shared-with-me"));

    fireEvent.click(screen.getByRole("button", { name: "By me" }));
    await waitFor(() => expect(seenFilters).toContain("by-me"));
  });

  it("toggles Show archived, which calls the API with include_archived and renders an Archived badge", async () => {
    const seenIncludeArchived: (string | null)[] = [];
    const archivedDashboard = { ...DASHBOARDS[0], id: 9, name: "Old dashboard", is_archived: true };
    server.use(
      http.get(`${BASE}/api/dashboards`, ({ request }) => {
        const url = new URL(request.url);
        seenIncludeArchived.push(url.searchParams.get("include_archived"));
        const includeArchived = url.searchParams.get("include_archived") === "true";
        return HttpResponse.json(includeArchived ? [...DASHBOARDS, archivedDashboard] : DASHBOARDS);
      }),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("Growth Overview");
    expect(seenIncludeArchived).toContain(null);
    expect(screen.queryByText("Old dashboard")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Show archived" }));

    await screen.findByText("Old dashboard");
    expect(seenIncludeArchived).toContain("true");
    expect(screen.getByText("Archived")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Show archived" }));
    await waitFor(() => expect(screen.queryByText("Old dashboard")).not.toBeInTheDocument());
    expect(seenIncludeArchived[seenIncludeArchived.length - 1]).toBe(null);
  });
});

describe("DashboardsPage Draft/Published chip", () => {
  it("renders a Draft chip for a never-published dashboard instead of its visibility tier", async () => {
    const neverPublished = [{ ...DASHBOARDS[0], published: false, has_unpublished_changes: false }];
    server.use(
      http.get(`${BASE}/api/dashboards`, () => HttpResponse.json(neverPublished)),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
    );

    renderPage();

    expect(await screen.findByText("Growth Overview")).toBeInTheDocument();
    expect(screen.getByText("Draft")).toBeInTheDocument();
    expect(screen.queryByText("Published")).not.toBeInTheDocument();
  });

  it("renders the visibility chip (not Draft) once the dashboard has been published", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards`, () => HttpResponse.json(DASHBOARDS)),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
    );

    renderPage();

    expect(await screen.findByText("Growth Overview")).toBeInTheDocument();
    expect(screen.getByText("Published")).toBeInTheDocument();
    expect(screen.queryByText("Draft")).not.toBeInTheDocument();
  });
});

describe("DashboardsPage row actions", () => {
  function mockList() {
    server.use(
      http.get(`${BASE}/api/dashboards`, () => HttpResponse.json(DASHBOARDS)),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
    );
  }

  it("exposes a Favorite toggle and a Delete action in the default table view", async () => {
    mockList();
    renderPage();
    await screen.findByText("Growth Overview");

    expect(screen.getByRole("button", { name: "Add to favorites" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
  });

  it("keeps the same Favorite/Delete actions after switching to the card view", async () => {
    mockList();
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Card view" }));

    expect(screen.getByRole("button", { name: "Add to favorites" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
  });

  it("toggling favorite calls the star/unstar endpoint for the row's id", async () => {
    mockList();
    let starred = false;
    server.use(
      http.post(`${BASE}/api/dashboards/1/favorite`, () => {
        starred = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Add to favorites" }));
    await waitFor(() => expect(starred).toBe(true));
  });

  it("deleting a dashboard opens a confirm dialog and calls DELETE on confirm", async () => {
    mockList();
    server.use(http.delete(`${BASE}/api/dashboards/growth-overview`, () => new HttpResponse(null, { status: 204 })));
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Growth Overview/)).toBeInTheDocument();

    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("a failed delete shows the reason inside the confirm dialog", async () => {
    mockList();
    server.use(
      http.delete(`${BASE}/api/dashboards/growth-overview`, () =>
        HttpResponse.json({ message: "Dashboard is embedded elsewhere" }, { status: 409 }),
      ),
    );
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    expect(await within(dialog).findByText("Dashboard is embedded elsewhere")).toBeInTheDocument();
  });

  it("cancelling after a failed delete clears the error", async () => {
    mockList();
    server.use(
      http.delete(`${BASE}/api/dashboards/growth-overview`, () =>
        HttpResponse.json({ message: "Dashboard is embedded elsewhere" }, { status: 409 }),
      ),
    );
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    let dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await within(dialog).findByText("Dashboard is embedded elsewhere");

    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    dialog = await screen.findByRole("dialog");
    expect(within(dialog).queryByText("Dashboard is embedded elsewhere")).not.toBeInTheDocument();
  });

  it("removes the deleted dashboard's detail cache entry so a stale slug can't reappear on back-navigation", async () => {
    mockList();
    server.use(http.delete(`${BASE}/api/dashboards/growth-overview`, () => new HttpResponse(null, { status: 204 })));

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["dashboard", "growth-overview"], DASHBOARDS[0]);
    const removeSpy = vi.spyOn(qc, "removeQueries");

    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter>
          <DashboardsPage />
        </MemoryRouter>
      </QueryClientProvider>,
    );
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(removeSpy).toHaveBeenCalledWith({ queryKey: ["dashboard", "growth-overview"] });
    expect(qc.getQueryData(["dashboard", "growth-overview"])).toBeUndefined();
  });

  it("shows an inline error when starring fails, instead of silently snapping back", async () => {
    mockList();
    server.use(
      http.post(`${BASE}/api/dashboards/1/favorite`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );
    renderPage();
    await screen.findByText("Growth Overview");

    await userEvent.click(screen.getByRole("button", { name: "Add to favorites" }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("does not render Delete for a row the caller can't edit", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards`, () =>
        HttpResponse.json([{ ...DASHBOARDS[0], caller: { object_role: "viewer", can_edit: false, can_delete: false } }]),
      ),
      http.get(`${BASE}/api/dashboards/tags`, () => HttpResponse.json({ tags: [] })),
    );
    renderPage();
    await screen.findByText("Growth Overview");

    expect(screen.queryByRole("button", { name: "Delete" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add to favorites" })).toBeInTheDocument();
  });
});
