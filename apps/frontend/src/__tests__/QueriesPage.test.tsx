import { describe, it, expect, vi, afterEach, beforeEach, beforeAll, afterAll } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import type * as ReactRouterModule from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { mockViewport } from "./mockViewport";
import { AuthProvider } from "@/lib/auth";
import { QueriesPage } from "@/pages/QueriesPage";

// Real route transitions break in jsdom/undici; assert navigate() instead.
const navigateSpy = vi.fn();
vi.mock("react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof ReactRouterModule>();
  return { ...actual, useNavigate: () => navigateSpy };
});

const BASE = "http://localhost:8080";

// setup.ts stubs matchMedia false (mobile cards view); pin desktop so tests see the table.
let restoreViewport: () => void;
beforeAll(() => {
  restoreViewport = mockViewport(true);
});
afterAll(() => {
  restoreViewport();
});

// Default /api/session user id 1 is "me".
const QUERIES = [
  {
    id: 1,
    name: "Signups by day",
    description: "",
    data_source_id: 1,
    user_id: 1,
    published: true,
    has_unpublished_changes: false,
    last_execution_status: "Success",
    last_execution_at: null,
    row_count: 10,
    latest_query_data_id: null,
    caller: { object_role: "owner", can_edit: true, can_delete: true },
  },
];

beforeEach(() => {
  navigateSpy.mockClear();
});

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter>
          <QueriesPage />
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("QueriesPage search debounce", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps the list rendered while typing instead of flashing the full-page loader", async () => {
    const seenSearchParams: (string | null)[] = [];
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        const url = new URL(request.url);
        seenSearchParams.push(url.searchParams.get("q"));
        return HttpResponse.json(QUERIES);
      }),
    );

    renderPage();
    expect(await screen.findByText("Signups by day")).toBeInTheDocument();

    vi.useFakeTimers();
    const input = screen.getByPlaceholderText("Search queries…");

    fireEvent.change(input, { target: { value: "s" } });
    fireEvent.change(input, { target: { value: "si" } });
    fireEvent.change(input, { target: { value: "sig" } });

    expect(screen.getByText("Signups by day")).toBeInTheDocument();
    expect(screen.queryByText(/loading your data/i)).not.toBeInTheDocument();

    act(() => {
      vi.advanceTimersByTime(300);
    });
    vi.useRealTimers();

    await waitFor(() => {
      expect(seenSearchParams).toContain("sig");
    });
    // One debounced request for the keystroke burst (the initial fetch has no "q").
    expect(seenSearchParams.filter((p) => p !== null).length).toBe(1);
  });
});

describe("QueriesPage owner + filters", () => {
  const ROWS = [
    {
      id: 1,
      name: "My query",
      description: "",
      data_source_id: 1,
      user_id: 1, // matches the session user from handlers.ts
      owner_name: "Test User",
      published: true,
      has_unpublished_changes: false,
      last_execution_status: "Success",
      last_execution_at: null,
      row_count: 10,
        latest_query_data_id: null,
      caller: { object_role: "owner", can_edit: true, can_delete: true },
    },
    {
      id: 2,
      name: "Someone else's draft",
      description: "",
      data_source_id: 1,
      user_id: 2,
      owner_name: "Other User",
      published: false,
      has_unpublished_changes: false,
      last_execution_status: "Success",
      last_execution_at: null,
      row_count: 0,
      latest_query_data_id: null,
      caller: { object_role: "viewer", can_edit: false, can_delete: false },
    },
  ];

  function mockList() {
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        const url = new URL(request.url);
        const filter = url.searchParams.get("filter");
        const rows = filter === "by-me" ? ROWS.filter((q) => q.user_id === 1) : ROWS;
        return HttpResponse.json(rows);
      }),
      http.get(`${BASE}/api/users`, () =>
        HttpResponse.json([
          { id: 1, name: "Test User", email: "test@example.com" },
          { id: 2, name: "Other User", email: "other@example.com" },
        ]),
      ),
    );
  }

  it("renders the owner name for each row", async () => {
    mockList();
    renderPage();
    expect(await screen.findByText("Test User")).toBeInTheDocument();
    expect(await screen.findByText("Other User")).toBeInTheDocument();
  });

  it("filters to only the current user's queries with By me", async () => {
    mockList();
    renderPage();
    await screen.findByText("My query");
    expect(screen.getByText("Someone else's draft")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "By me" }));

    await waitFor(() => {
      expect(screen.queryByText("Someone else's draft")).not.toBeInTheDocument();
    });
    expect(screen.getByText("My query")).toBeInTheDocument();
  });

  it("filters by Draft / Published status", async () => {
    mockList();
    renderPage();
    await screen.findByText("My query");

    fireEvent.click(screen.getByRole("button", { name: "Draft" }));
    expect(screen.queryByText("My query")).not.toBeInTheDocument();
    expect(screen.getByText("Someone else's draft")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Published" }));
    expect(screen.getByText("My query")).toBeInTheDocument();
    expect(screen.queryByText("Someone else's draft")).not.toBeInTheDocument();
  });

  it("shows the schedule cadence label from the schedules-list API", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/schedules`, () =>
        HttpResponse.json([
          {
            id: 1,
            entity_type: "query",
            entity_id: 1,
            spec: { kind: "interval", every_secs: 900 },
            timezone: "UTC",
            enabled: true,
            paused_until: null,
            auto_paused_at: null,
            overlap_policy: "skip",
            next_run_at: new Date().toISOString(),
            consecutive_failures: 0,
            last_run: null,
          },
        ]),
      ),
    );
    renderPage();
    await screen.findByText("My query");
    expect(await screen.findByText("Every 15 min")).toBeInTheDocument();
    expect(screen.queryByText("Scheduled (inactive)")).not.toBeInTheDocument();
    expect(screen.queryByText("Scheduled")).not.toBeInTheDocument();
  });

  it("shows 'Paused' for a schedule paused only via a future paused_until (no auto_paused_at)", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/schedules`, () =>
        HttpResponse.json([
          {
            id: 1,
            entity_type: "query",
            entity_id: 1,
            spec: { kind: "interval", every_secs: 900 },
            timezone: "UTC",
            enabled: true,
            paused_until: new Date(Date.now() + 60_000).toISOString(),
            auto_paused_at: null,
            overlap_policy: "skip",
            next_run_at: new Date().toISOString(),
            consecutive_failures: 0,
            last_run: null,
          },
        ]),
      ),
    );
    renderPage();
    await screen.findByText("My query");
    expect(await screen.findByText("Paused")).toBeInTheDocument();
    expect(screen.queryByText("Every 15 min")).not.toBeInTheDocument();
  });

  it("shows an owner chip labeled 'You' for the current user's own row and a visibility/draft chip per row", async () => {
    mockList();
    renderPage();
    await screen.findByText("My query");

    expect(screen.getByText("You")).toBeInTheDocument();
    expect(screen.getByText("Other User")).toBeInTheDocument();
    // "Draft"/"Published" also label status-filter buttons; the badge is the second match.
    expect(screen.getAllByText("Draft")).toHaveLength(2);
    expect(screen.getAllByText("Published")).toHaveLength(2);
  });

  it("shows 'Unpublished changes' only for the published-with-pending-edits row", async () => {
    const rows = [
      { ...ROWS[0], id: 3, name: "Never published", published: false, has_unpublished_changes: false },
      { ...ROWS[0], id: 4, name: "Published, clean", published: true, has_unpublished_changes: false },
      { ...ROWS[0], id: 5, name: "Published, pending", published: true, has_unpublished_changes: true },
    ];
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json(rows)),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("Never published");
    await screen.findByText("Published, clean");
    await screen.findByText("Published, pending");

    expect(screen.getAllByText("Unpublished changes")).toHaveLength(1);
  });

  it("wires the scope toggle to the ?filter= param", async () => {
    const seenFilters: (string | null)[] = [];
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        seenFilters.push(new URL(request.url).searchParams.get("filter"));
        return HttpResponse.json(ROWS);
      }),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("My query");
    expect(seenFilters).toContain(null);

    fireEvent.click(screen.getByRole("button", { name: "Shared with me" }));
    await waitFor(() => expect(seenFilters).toContain("shared-with-me"));
  });

  it("toggles Show archived, which calls the API with include_archived and renders an Archived badge", async () => {
    const seenIncludeArchived: (string | null)[] = [];
    const archivedRow = { ...ROWS[0], id: 9, name: "Old query", is_archived: true };
    server.use(
      http.get(`${BASE}/api/queries`, ({ request }) => {
        const url = new URL(request.url);
        seenIncludeArchived.push(url.searchParams.get("include_archived"));
        const includeArchived = url.searchParams.get("include_archived") === "true";
        return HttpResponse.json(includeArchived ? [...ROWS, archivedRow] : ROWS);
      }),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );

    renderPage();
    await screen.findByText("My query");
    expect(seenIncludeArchived).toContain("false");
    expect(screen.queryByText("Old query")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Show archived" }));

    await screen.findByText("Old query");
    expect(seenIncludeArchived).toContain("true");
    expect(screen.getByText("Archived")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Show archived" }));
    await waitFor(() => expect(screen.queryByText("Old query")).not.toBeInTheDocument());
    expect(seenIncludeArchived[seenIncludeArchived.length - 1]).toBe("false");
  });
});

describe("QueriesPage tags + description", () => {
  const ROWS = [
    {
      id: 1, name: "Tagged query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 10, latest_query_data_id: null, tags: ["revenue"],
      caller: { object_role: "owner", can_edit: true, can_delete: true },
    },
    {
      id: 2, name: "Untagged query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 10, latest_query_data_id: null, tags: [],
      caller: { object_role: "owner", can_edit: true, can_delete: true },
    },
  ];

  it("renders a tag pill from GET /api/queries/tags, filters rows, and clears", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json(ROWS)),
      http.get(`${BASE}/api/queries/tags`, () => HttpResponse.json({ tags: ["revenue"] })),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Tagged query");
    expect(screen.getByText("Untagged query")).toBeInTheDocument();

    const pill = screen.getByRole("button", { name: "revenue" });
    fireEvent.click(pill);

    await waitFor(() => expect(screen.queryByText("Untagged query")).not.toBeInTheDocument());
    expect(screen.getByText("Tagged query")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "clear" }));
    await waitFor(() => expect(screen.getByText("Untagged query")).toBeInTheDocument());
  });

  it("renders a long description truncated with the full text reachable via the tooltip", async () => {
    const longDescription =
      "A very long description that should be truncated in the list view but fully visible in the tooltip content when hovered or focused by the user.";
    const rows = [{ ...ROWS[1], id: 3, name: "Described query", description: longDescription }];
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json(rows)),
      http.get(`${BASE}/api/queries/tags`, () => HttpResponse.json({ tags: [] })),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Described query");

    const trigger = screen.getByText(longDescription);
    expect(trigger).toHaveClass("truncate");
    expect(await screen.findByText(longDescription)).toBeInTheDocument();
  });

  it("renders no description tooltip trigger when a query has no description", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROWS[1]])),
      http.get(`${BASE}/api/queries/tags`, () => HttpResponse.json({ tags: [] })),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Untagged query");
    expect(screen.queryByText(/^A very long description/)).not.toBeInTheDocument();
  });
});

describe("QueriesPage run-button gating", () => {
  const ROWS = [
    {
      id: 1, name: "Runnable query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 10, latest_query_data_id: null,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
    },
    {
      id: 2, name: "Locked query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 10, latest_query_data_id: null,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: false, execute_blocked_reason: "capability" },
    },
  ];

  it("renders a Run button for runnable rows and a locked affordance for blocked rows", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json(ROWS)),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Runnable query");

    expect(screen.getByRole("button", { name: "Run" })).toBeInTheDocument();
    expect(
      screen.getByTitle("Limited access — some actions unavailable. Ask an admin."),
    ).toBeInTheDocument();
  });
});

describe("QueriesPage row Stop/cancel", () => {
  const ROW = {
    id: 1, name: "Runnable query", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false, last_execution_status: "Success",
    last_execution_at: null, row_count: 10, latest_query_data_id: null,
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };

  /** Mirrors `JobStatus` in types/api.ts; the row test drives the real usePollJob. */
  const PENDING = 1;
  const CANCELLED = 5;

  it("cancelling a row refresh does not render 'Query run failed'", async () => {
    let jobStatus = PENDING;
    // Real usePollJob backs off between polls; allow more than the 5s default.
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROW])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/queries/1/refresh`, () =>
        HttpResponse.json({ job: { id: "job-1", status: "pending" } }),
      ),
      http.get(`${BASE}/api/jobs/job-1`, () =>
        HttpResponse.json({
          job: { id: "job-1", status: jobStatus, query_result_id: null, error: null },
        }),
      ),
      http.delete(`${BASE}/api/jobs/job-1`, () => {
        jobStatus = CANCELLED;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderPage();
    await screen.findByText("Runnable query");

    await userEvent.click(screen.getByRole("button", { name: "Run" }));
    const stopBtn = await screen.findByRole("button", { name: "Stop run" });
    await userEvent.click(stopBtn);

    await waitFor(() => expect(screen.getByRole("button", { name: "Run" })).toBeInTheDocument(), {
      timeout: 8000,
    });
    expect(screen.queryByText("Query run failed")).not.toBeInTheDocument();
  }, 10000);
});

describe("QueriesPage row semantics (tr, not button-wrapping-td)", () => {
  const ROW = {
    id: 1, name: "My query", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false, last_execution_status: "Success",
    last_execution_at: null, row_count: 10, latest_query_data_id: 99,
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };

  function mockList() {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROW])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
  }

  it("keeps the native table-row role on each row instead of overriding it to role=button", async () => {
    mockList();
    renderPage();
    await screen.findByText("My query");

    // Overriding the row with role="button" would hide it from this table query.
    expect(screen.getAllByRole("row")).toHaveLength(2);
    expect(screen.getByRole("link", { name: "My query" })).toHaveAttribute("href", "/queries/1");
  });

  it("clicking the row navigates to the query, but clicking an action button inside it doesn't", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 0, dashboard_count: 0, monitor_count: 0 }),
      ),
    );
    renderPage();
    await screen.findByText("My query");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(navigateSpy).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Cancel" }));

    await userEvent.click(screen.getByText("10")); // the row-count cell
    expect(navigateSpy).toHaveBeenCalledWith("/queries/1");
  });
});

describe("QueriesPage export/result-availability display", () => {
  const BASE_ROW = {
    id: 1, name: "Weekly signups", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false, last_execution_status: "Success",
    last_execution_at: "2026-01-01T00:00:00Z", row_count: 5, latest_query_data_id: null,
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };

  it("shows 'Result unavailable' instead of Success+row-count when a Success row has no reachable result pointer, and keeps Export disabled", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([BASE_ROW])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Weekly signups");

    expect(screen.getByText("Result unavailable")).toBeInTheDocument();
    expect(screen.queryByText("Success")).not.toBeInTheDocument();
    expect(screen.queryByText("5")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /export/i })).toBeDisabled();
  });

  it("shows the normal Success status and row count once the result pointer is set", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () =>
        HttpResponse.json([{ ...BASE_ROW, latest_query_data_id: 245 }]),
      ),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Weekly signups");

    expect(screen.getByText("Success")).toBeInTheDocument();
    expect(screen.getByText("5")).toBeInTheDocument();
    expect(screen.queryByText("Result unavailable")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /export/i })).not.toBeDisabled();
  });
});

describe("QueriesPage delete dialog impact warning", () => {
  const ROW = {
    id: 1, name: "My query", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false, last_execution_status: "Success",
    last_execution_at: null, row_count: 10, latest_query_data_id: null,
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };

  function mockList() {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROW])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
  }

  async function openDeleteDialog() {
    renderPage();
    await screen.findByText("My query");
    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    return screen.findByRole("dialog");
  }

  it("fetches consumers and renders the tile/dashboard/monitor counts", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 3, dashboard_count: 2, monitor_count: 1 }),
      ),
    );

    const dialog = await openDeleteDialog();

    expect(
      await within(dialog).findByText("Feeds 3 tiles on 2 dashboards · 1 monitor. Deleting this query removes them."),
    ).toBeInTheDocument();
  });

  it("renders no warning when all consumer counts are zero", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 0, dashboard_count: 0, monitor_count: 0 }),
      ),
    );

    const dialog = await openDeleteDialog();

    await waitFor(() => {
      expect(within(dialog).queryByText(/Deleting this query removes them/)).not.toBeInTheDocument();
    });
  });

  it("still allows deleting when the consumers fetch fails, with no error text shown", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ message: "boom" }, { status: 500 }),
      ),
      http.delete(`${BASE}/api/queries/1`, () => new HttpResponse(null, { status: 204 })),
    );

    const dialog = await openDeleteDialog();

    const deleteBtn = within(dialog).getByRole("button", { name: "Delete" });
    await waitFor(() => expect(deleteBtn).not.toBeDisabled());
    expect(within(dialog).queryByText(/removes them/)).not.toBeInTheDocument();
    await userEvent.click(deleteBtn);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("invalidates queries, dashboards, monitors, and schedules on a successful delete", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 0, dashboard_count: 0, monitor_count: 0 }),
      ),
      http.delete(`${BASE}/api/queries/1`, () => new HttpResponse(null, { status: 204 })),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidateSpy = vi.spyOn(qc, "invalidateQueries");

    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter>
            <QueriesPage />
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
    await screen.findByText("My query");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const invalidatedKeys = invalidateSpy.mock.calls.map((call) => (call[0] as { queryKey: unknown[] }).queryKey[0]);
    expect(invalidatedKeys).toEqual(
      expect.arrayContaining(["queries", "query-tags", "dashboards", "monitors", "schedules"]),
    );
  });

  it("refetches the tag list after a successful delete", async () => {
    mockList();
    let tagsCalls = 0;
    server.use(
      http.get(`${BASE}/api/queries/tags`, () => {
        tagsCalls += 1;
        return HttpResponse.json({ tags: ["revenue"] });
      }),
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 0, dashboard_count: 0, monitor_count: 0 }),
      ),
      http.delete(`${BASE}/api/queries/1`, () => new HttpResponse(null, { status: 204 })),
    );

    renderPage();
    await screen.findByText("My query");
    await waitFor(() => expect(tagsCalls).toBeGreaterThan(0));
    const callsBeforeDelete = tagsCalls;

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(tagsCalls).toBeGreaterThan(callsBeforeDelete));
  });

  it("removes the deleted query's detail cache entry so a stale row can't reappear on back-navigation", async () => {
    mockList();
    server.use(
      http.get(`${BASE}/api/queries/1/consumers`, () =>
        HttpResponse.json({ tile_count: 0, dashboard_count: 0, monitor_count: 0 }),
      ),
      http.delete(`${BASE}/api/queries/1`, () => new HttpResponse(null, { status: 204 })),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    qc.setQueryData(["query", 1], ROW);
    const removeSpy = vi.spyOn(qc, "removeQueries");

    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter>
            <QueriesPage />
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
    await screen.findByText("My query");

    await userEvent.click(screen.getByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(removeSpy).toHaveBeenCalledWith({ queryKey: ["query", 1] });
    expect(qc.getQueryData(["query", 1])).toBeUndefined();
  });
});

describe("QueriesPage favorites", () => {
  const ROWS = [
    {
      id: 1, name: "Starred query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 10, latest_query_data_id: null, is_favorite: true,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
    },
    {
      id: 2, name: "Plain query", description: "", data_source_id: 1, user_id: 1,
      published: true, has_unpublished_changes: false, last_execution_status: "Success",
      last_execution_at: null, row_count: 0, latest_query_data_id: null, is_favorite: false,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
    },
  ];

  function mockList() {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json(ROWS)),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
  }

  it("filters to favorited-only rows with the Favorites toggle", async () => {
    mockList();
    renderPage();
    await screen.findByText("Starred query");
    expect(screen.getByText("Plain query")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Favorites" }));
    await waitFor(() => expect(screen.queryByText("Plain query")).not.toBeInTheDocument());
    expect(screen.getByText("Starred query")).toBeInTheDocument();
  });

  it("shows the filtered-empty state (not the first-query empty state) when Favorites matches nothing, and Clear filters resets it", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROWS[1]])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Plain query");

    fireEvent.click(screen.getByRole("button", { name: "Favorites" }));

    expect(await screen.findByText("No queries match these filters")).toBeInTheDocument();
    expect(screen.queryByText("Write your first query")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Clear filters" }));
    await waitFor(() => expect(screen.getByText("Plain query")).toBeInTheDocument());
    expect(screen.getByRole("button", { name: "Favorites" })).toHaveAttribute("aria-pressed", "false");
  });

  it("star button calls the star/unstar endpoint for the row's id and invalidates the list", async () => {
    mockList();
    let starCalled = false;
    let unstarCalled = false;
    server.use(
      http.post(`${BASE}/api/queries/2/favorite`, () => {
        starCalled = true;
        return new HttpResponse(null, { status: 204 });
      }),
      http.delete(`${BASE}/api/queries/1/favorite`, () => {
        unstarCalled = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPage();
    await screen.findByText("Starred query");

    const addButtons = screen.getAllByRole("button", { name: "Add to favorites" });
    expect(addButtons).toHaveLength(1);
    await userEvent.click(addButtons[0]);
    await waitFor(() => expect(starCalled).toBe(true));

    await userEvent.click(screen.getByRole("button", { name: "Remove from favorites" }));
    await waitFor(() => expect(unstarCalled).toBe(true));
  });

  it("shows an inline error when starring fails, instead of silently snapping back", async () => {
    mockList();
    server.use(
      http.post(`${BASE}/api/queries/2/favorite`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );
    renderPage();
    await screen.findByText("Plain query");

    const addButtons = screen.getAllByRole("button", { name: "Add to favorites" });
    await userEvent.click(addButtons[0]);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("shows the star button regardless of can_edit", async () => {
    server.use(
      http.get(`${BASE}/api/queries`, () =>
        HttpResponse.json([
          { ...ROWS[1], caller: { object_role: "viewer", can_edit: false, can_delete: false, can_execute: true } },
        ]),
      ),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Plain query");

    expect(screen.getByRole("button", { name: "Add to favorites" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
  });
});

describe("QueriesPage table/cards view toggle", () => {
  const ROW = {
    id: 1, name: "My query", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false, last_execution_status: "Success",
    last_execution_at: null, row_count: 10, latest_query_data_id: 99,
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };
  const STORAGE_KEY = "list-view:queries";

  beforeEach(() => {
    window.localStorage.removeItem(STORAGE_KEY);
    server.use(
      http.get(`${BASE}/api/queries`, () => HttpResponse.json([ROW])),
      http.get(`${BASE}/api/users`, () => HttpResponse.json([])),
    );
  });

  it("renders the table by default when nothing is stored", async () => {
    renderPage();
    await screen.findByText("My query");
    expect(screen.getAllByRole("row")).toHaveLength(2);
    expect(screen.getByRole("button", { name: "Card view" })).toHaveAttribute("aria-pressed", "false");
  });

  it("switches to the card layout and persists the choice to localStorage, keeping the same actions available", async () => {
    renderPage();
    await screen.findByText("My query");

    await userEvent.click(screen.getByRole("button", { name: "Card view" }));

    expect(screen.queryAllByRole("row")).toHaveLength(0);
    expect(window.localStorage.getItem(STORAGE_KEY)).toBe("cards");
    expect(screen.getByRole("button", { name: "Run" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
  });

  it("forces the card layout below md regardless of a stored 'table' preference", async () => {
    window.localStorage.setItem(STORAGE_KEY, "table");
    restoreViewport(); // undo the file-level desktop pin for this one test
    const restoreMobile = mockViewport(false);
    try {
      renderPage();
      await screen.findByText("My query");
      expect(screen.queryAllByRole("row")).toHaveLength(0);
      expect(screen.queryByRole("button", { name: "Card view" })).not.toBeInTheDocument();
    } finally {
      restoreMobile();
      restoreViewport = mockViewport(true);
    }
  });
});
