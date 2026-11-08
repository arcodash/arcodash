/** Real-router leave-guard coverage: react-router is not mocked, so the real `useBlocker` engages. */
import { describe, it, expect, vi } from "vitest";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider, useQuery } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: vi.fn(({ value, onChange, className, readOnly }) => (
    <textarea
      data-testid="sql-editor"
      className={className}
      value={value}
      disabled={readOnly}
      onChange={(e) => onChange?.(e.target.value)}
    />
  )),
}));
vi.mock("@/components/query-editor/SchemaBrowser", () => ({
  SchemaBrowser: () => <div data-testid="schema-browser" />,
}));
vi.mock("@/components/data-sources/SchemaBrowser", () => ({
  SchemaBrowser: () => <div data-testid="schema-browser" />,
}));
vi.mock("@/components/query-editor/SnippetMenu", () => ({
  SnippetMenu: () => <div data-testid="snippet-menu" />,
}));
vi.mock("@/components/query-editor/VersionHistoryPanel", () => ({
  VersionHistoryPanel: () => <div data-testid="version-history" />,
}));
vi.mock("@/components/scheduling/FreshnessChip", () => ({
  FreshnessChip: () => <div data-testid="freshness-chip" />,
}));
vi.mock("@/components/scheduling/ScheduleSection", () => ({
  ScheduleSection: () => <div data-testid="schedule-section" />,
  QUERY_FLAVOR: {},
}));
vi.mock("@/components/query-editor/ExportMenu", () => ({
  ExportMenu: () => <div data-testid="export-menu" />,
}));
vi.mock("@/components/visualizations/VizPanel", () => ({
  VizPanel: () => <div data-testid="viz-panel" />,
}));
vi.mock("@/lib/jobs", () => ({
  usePollJob: vi.fn(() => ({
    isSuccess: false,
    isDone: false,
    isPending: false,
    isStarted: false,
    isFailure: false,
    isCancelled: false,
    error: null,
    queryResultId: null,
  })),
}));

import { QueryEditorPage } from "@/pages/QueryEditorPage";

const BASE = "http://localhost:8080";
const DS = { id: 1, name: "Postgres", type: "pg", paused: false, pause_reason: null, schema_refreshed_at: null, created_at: "", updated_at: "" };
const QUERY = {
  id: 42, name: "My query", query: "SELECT 1", data_source_id: 1,
  options: { parameters: [] }, is_archived: false,
  published: false, has_unpublished_changes: false, working_version: 1,
  visibility: "workspace",
  is_favorite: false, version: 1,
  description: "", user_id: 1, owner_name: "Ada Lovelace", latest_query_data_id: null,
  last_execution_status: "New" as const, last_execution_at: null,
  created_at: "", updated_at: "",
  caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
};

function renderAt(path: string) {
  server.use(
    http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
    http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
    http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
  );
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createMemoryRouter(
    [
      { path: "/queries/:id", element: <QueryEditorPage /> },
      { path: "/queries", element: <div>Queries list</div> },
    ],
    { initialEntries: [path] },
  );
  render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
  return { router, queryClient: qc };
}

describe("QueryEditorPage leave-guard on /queries/new", () => {
  it("Execute creates the row, the URL actually swaps, no blocker engages, and a later name edit PATCHes instead of duplicating", async () => {
    const createBodies: Record<string, unknown>[] = [];
    let updateBody: Record<string, unknown> | null = null;
    let execBody: Record<string, unknown> | null = null;
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        createBodies.push(body);
        return HttpResponse.json({ ...QUERY, id: 77, name: body.name, query: body.query, working_version: 1 });
      }),
      http.get(`${BASE}/api/queries/77`, () =>
        HttpResponse.json({ ...QUERY, id: 77, name: "Untitled query", query: "SELECT * FROM widgets", working_version: 1 }),
      ),
      http.post(`${BASE}/api/queries/77`, async ({ request }) => {
        updateBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ ...QUERY, id: 77, name: updateBody.name as string, query: updateBody.query as string, working_version: 2 });
      }),
      http.post(`${BASE}/api/query_results`, async ({ request }) => {
        execBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT * FROM widgets",
            data: { columns: [], rows: [], runtime_seconds: 0.01 },
            runtime: 0.01,
            retrieved_at: new Date().toISOString(),
          },
        });
      }),
    );

    const { router } = renderAt("/queries/new");

    await screen.findByDisplayValue("Untitled query");
    const editor = await screen.findByTestId("sql-editor");
    const execBtn = await screen.findByRole("button", { name: "Execute" });
    await waitFor(() => expect(execBtn).not.toBeDisabled());

    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
    fireEvent.click(execBtn);

    await waitFor(() => expect(createBodies).toHaveLength(1));
    await waitFor(() => expect(execBody).not.toBeNull());
    expect((execBody as unknown as Record<string, unknown>).query_id).toBe(77);

    await waitFor(() => expect(router.state.location.pathname).toBe("/queries/77"));
    expect(router.state.blockers.size).toBe(0);
    expect(screen.queryByText("Leave without saving?")).not.toBeInTheDocument();

    const renamedInput = await screen.findByDisplayValue("Untitled query");
    vi.useFakeTimers();
    fireEvent.change(renamedInput, { target: { value: "Renamed draft" } });
    await vi.advanceTimersByTimeAsync(1500);
    vi.useRealTimers();

    await waitFor(() => expect(updateBody).not.toBeNull());
    expect(createBodies).toHaveLength(1);
  });

  it("blocks navigation to an unrelated route in the window right after a create lands, while still allowing the create's own URL swap", async () => {
    const createBodies: Record<string, unknown>[] = [];
    // The create awaits invalidateQueries before navigate(); holding that refetch open (active
    // subscriber on the key) widens the window to drive a second navigation into it.
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        createBodies.push(body);
        return HttpResponse.json({ ...QUERY, id: 77, name: body.name, query: body.query, working_version: 1 });
      }),
      http.get(`${BASE}/api/queries`, () => new Promise<never>(() => {
      })),
      http.get(`${BASE}/api/queries/77`, () =>
        HttpResponse.json({ ...QUERY, id: 77, name: "Untitled query", query: "SELECT * FROM widgets", working_version: 1 }),
      ),
      http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT * FROM widgets",
            data: { columns: [], rows: [], runtime_seconds: 0.01 },
            runtime: 0.01,
            retrieved_at: new Date().toISOString(),
          },
        }),
      ),
    );

    function ListSubscriber() {
      useQuery({
        queryKey: ["queries"],
        queryFn: () => fetch(`${BASE}/api/queries`, { credentials: "include" }).then((r) => r.json()),
      });
      return null;
    }

    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
    );
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const router = createMemoryRouter(
      [
        { path: "/queries/:id", element: <><ListSubscriber /><QueryEditorPage /></> },
        { path: "/queries", element: <div>Queries list</div> },
      ],
      { initialEntries: ["/queries/new"] },
    );
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <RouterProvider router={router} />
        </AuthProvider>
      </QueryClientProvider>,
    );

    const editor = await screen.findByTestId("sql-editor");
    const execBtn = await screen.findByRole("button", { name: "Execute" });
    await waitFor(() => expect(execBtn).not.toBeDisabled());

    // canPersist stays false, so hasUnsavedNewDraft is true between the create landing and the
    // route transition.
    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
    fireEvent.click(execBtn);
    await waitFor(() => expect(createBodies).toHaveLength(1));

    // The create's navigate() is stuck behind the pending refetch; race a navigation to another
    // route into that window.
    act(() => {
      router.navigate("/queries");
    });

    await waitFor(() => expect(router.state.blockers.size).toBe(1));
    expect(router.state.location.pathname).not.toBe("/queries");
    expect(await screen.findByText("Leave without saving?")).toBeInTheDocument();
  });

  it("still blocks a genuinely-unsaved new draft (real SQL, no row created) from navigating away", async () => {
    const { router } = renderAt("/queries/new");

    await screen.findByDisplayValue("Untitled query");
    const editor = await screen.findByTestId("sql-editor");

    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
    await waitFor(() => expect(editor).toHaveValue("SELECT * FROM widgets"));

    act(() => {
      router.navigate("/queries");
    });

    await waitFor(() => expect(router.state.blockers.size).toBe(1));
    expect(router.state.location.pathname).toBe("/queries/new");
    expect(await screen.findByText("Leave without saving?")).toBeInTheDocument();
  });

  it("unmount-flush still creates the row (cache write is unconditional) but does not navigate back into a route the user already left", async () => {
    const createBodies: Record<string, unknown>[] = [];
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        createBodies.push(body);
        return HttpResponse.json({ ...QUERY, id: 88, name: body.name as string, query: body.query as string, working_version: 1 });
      }),
    );

    const { router } = renderAt("/queries/new");

    const nameInput = await screen.findByDisplayValue("Untitled query");
    const editor = await screen.findByTestId("sql-editor");

    fireEvent.change(nameInput, { target: { value: "Draft about to be abandoned" } });
    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });

    // Leave well inside the debounce window; canPersist is true, so the guard doesn't block.
    act(() => {
      router.navigate("/queries");
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/queries"));
    expect(screen.getByText("Queries list")).toBeInTheDocument();

    await waitFor(() => expect(createBodies).toHaveLength(1));
    expect(createBodies[0].name).toBe("Draft about to be abandoned");

    expect(router.state.location.pathname).toBe("/queries");
    expect(screen.queryByText("Draft about to be abandoned")).not.toBeInTheDocument();
  });
});

describe("QueryEditorPage leave-guard on an existing query mid-autosave", () => {
  it("blocks in-app navigation while a save is genuinely stuck in flight, not merely dispatched", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      // A save that never resolves; the guard must still fire.
      http.post(`${BASE}/api/queries/42`, () => new Promise<never>(() => {})),
    );

    const { router } = renderAt("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 1, 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByText("Saving…");

    act(() => {
      router.navigate("/queries");
    });

    await waitFor(() => expect(router.state.blockers.size).toBe(1));
    expect(router.state.location.pathname).toBe("/queries/42");
    expect(await screen.findByText("Leave without saving?")).toBeInTheDocument();
  });

  it("does not block in-app navigation once the in-flight save has actually resolved", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, query: "SELECT 1, 2", working_version: 2 }),
      ),
    );

    const { router } = renderAt("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 1, 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByText("Saved");

    act(() => {
      router.navigate("/queries");
    });

    await waitFor(() => expect(router.state.location.pathname).toBe("/queries"));
    expect(router.state.blockers.size).toBe(0);
    expect(screen.queryByText("Leave without saving?")).not.toBeInTheDocument();
  });

  it("does not spuriously block navigation while an edit is still sitting in the debounce window", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, query: "SELECT 1, 2", working_version: 2 }),
      ),
    );

    const { router } = renderAt("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    fireEvent.change(editor, { target: { value: "SELECT 1, 2" } });

    act(() => {
      router.navigate("/queries");
    });

    await waitFor(() => expect(router.state.location.pathname).toBe("/queries"));
    expect(router.state.blockers.size).toBe(0);
  });
});
