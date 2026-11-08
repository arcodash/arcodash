/** persistBuffer's create must not pick create-vs-update from `queryId` alone: a save chained off
 * the create's success handler runs before navigate() re-renders with the new id, and would POST a
 * second create. */
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, waitFor, act } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: vi.fn(({ value, onChange, className, readOnly }: { value: string; onChange?: (v: string) => void; className?: string; readOnly?: boolean }) => (
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

function renderNew() {
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
    { initialEntries: ["/queries/new"] },
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

// Waits on real timers for fields and a loaded data source (needed for canPersist); the caller then
// edits under fake timers so the debounce uses the fake clock.
async function waitUntilReady() {
  const nameInput = await screen.findByDisplayValue("Untitled query");
  const editor = await screen.findByTestId("sql-editor");
  const execBtn = await screen.findByRole("button", { name: "Execute" });
  await waitFor(() => expect(execBtn).not.toBeDisabled());
  return { nameInput, editor };
}

describe("QueryEditorPage autosave — create/chain race", () => {
  it("a keystroke during the create POST updates the new row instead of creating a second one", async () => {
    let createCalls = 0;
    let resolveCreate!: () => void;
    const createGate = new Promise<void>((resolve) => {
      resolveCreate = resolve;
    });
    const updateBodies: Record<string, unknown>[] = [];
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        createCalls += 1;
        const body = (await request.json()) as Record<string, unknown>;
        await createGate;
        return HttpResponse.json({ ...QUERY, id: 77, name: body.name, query: body.query, working_version: 1 });
      }),
      http.get(`${BASE}/api/queries/77`, () =>
        HttpResponse.json({ ...QUERY, id: 77, name: "first draft", query: "SELECT 1 FROM a", working_version: 1 }),
      ),
      http.post(`${BASE}/api/queries/77`, async ({ request }) => {
        updateBodies.push((await request.json()) as Record<string, unknown>);
        return HttpResponse.json({ ...QUERY, id: 77, name: "first draft", query: "SELECT 2 FROM a", working_version: 2 });
      }),
    );

    renderNew();
    const { nameInput, editor } = await waitUntilReady();

    vi.useFakeTimers();
    fireEvent.change(nameInput, { target: { value: "first draft" } });
    fireEvent.change(editor, { target: { value: "SELECT 1 FROM a" } });
    await vi.advanceTimersByTimeAsync(1100);
    expect(createCalls).toBe(1);

    fireEvent.change(editor, { target: { value: "SELECT 2 FROM a" } });
    vi.useRealTimers();

    resolveCreate();

    await waitFor(() => expect(updateBodies).toHaveLength(1));
    expect(createCalls).toBe(1);
    expect(updateBodies[0].query).toBe("SELECT 2 FROM a");
  });

  it("the chained save targets the created id even though the URL swap has not rendered yet", async () => {
    let resolveCreate!: () => void;
    const createGate = new Promise<void>((resolve) => {
      resolveCreate = resolve;
    });
    let updateCallsOn91 = 0;
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        await createGate;
        return HttpResponse.json({ ...QUERY, id: 91, name: body.name, query: body.query, working_version: 1 });
      }),
      http.get(`${BASE}/api/queries/91`, () =>
        HttpResponse.json({ ...QUERY, id: 91, name: "second draft", query: "SELECT 1 FROM b", working_version: 1 }),
      ),
      http.post(`${BASE}/api/queries/91`, () => {
        updateCallsOn91 += 1;
        return HttpResponse.json({ ...QUERY, id: 91, name: "second draft", query: "SELECT 2 FROM b", working_version: 2 });
      }),
    );

    renderNew();
    const { nameInput, editor } = await waitUntilReady();

    vi.useFakeTimers();
    fireEvent.change(nameInput, { target: { value: "second draft" } });
    fireEvent.change(editor, { target: { value: "SELECT 1 FROM b" } });
    await vi.advanceTimersByTimeAsync(1100);
    fireEvent.change(editor, { target: { value: "SELECT 2 FROM b" } });
    vi.useRealTimers();

    resolveCreate();

    await waitFor(() => expect(updateCallsOn91).toBe(1));
  });

  it("a second new-query draft does not reuse the previous draft's id", async () => {
    const createBodies: Record<string, unknown>[] = [];
    server.use(
      http.post(`${BASE}/api/queries`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        createBodies.push(body);
        const id = createBodies.length === 1 ? 77 : 88;
        return HttpResponse.json({ ...QUERY, id, name: body.name, query: body.query, working_version: 1 });
      }),
      http.get(`${BASE}/api/queries/77`, () =>
        HttpResponse.json({ ...QUERY, id: 77, name: "draft one", query: "SELECT 1 FROM c", working_version: 1 }),
      ),
    );

    const { router } = renderNew();
    const { nameInput, editor } = await waitUntilReady();

    vi.useFakeTimers();
    fireEvent.change(nameInput, { target: { value: "draft one" } });
    fireEvent.change(editor, { target: { value: "SELECT 1 FROM c" } });
    await vi.advanceTimersByTimeAsync(1100);
    vi.useRealTimers();

    await waitFor(() => expect(createBodies).toHaveLength(1));
    await waitFor(() => expect(router.state.location.pathname).toBe("/queries/77"));

    act(() => {
      router.navigate("/queries/new");
    });
    await waitFor(() => expect(router.state.location.pathname).toBe("/queries/new"));

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 1 FROM d" } });
    await vi.advanceTimersByTimeAsync(1100);
    vi.useRealTimers();

    await waitFor(() => expect(createBodies).toHaveLength(2));
    expect(createBodies[1].query).toBe("SELECT 1 FROM d");
  });
});
