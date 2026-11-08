/** StrictMode's double mount/cleanup/mount must restore `mountedRef`, or the create path's guarded
 * navigate() no-ops and every later autosave re-POSTs. */
import React from "react";
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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

function renderStrictAt(path: string) {
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
    <React.StrictMode>
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <RouterProvider router={router} />
        </AuthProvider>
      </QueryClientProvider>
    </React.StrictMode>,
  );
  return router;
}

describe("QueryEditorPage under StrictMode", () => {
  it("swaps the URL after the create autosave, then PATCHes on the next edit instead of creating a duplicate", async () => {
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

    const router = renderStrictAt("/queries/new");

    await screen.findByDisplayValue("Untitled query");
    const editor = await screen.findByTestId("sql-editor");
    const execBtn = await screen.findByRole("button", { name: "Execute" });
    await waitFor(() => expect(execBtn).not.toBeDisabled());

    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
    fireEvent.click(execBtn);

    await waitFor(() => expect(createBodies).toHaveLength(1));
    await waitFor(() => expect(execBody).not.toBeNull());

    await waitFor(() => expect(router.state.location.pathname).toBe("/queries/77"));

    const renamedInput = await screen.findByDisplayValue("Untitled query");
    vi.useFakeTimers();
    fireEvent.change(renamedInput, { target: { value: "Renamed draft" } });
    await vi.advanceTimersByTimeAsync(1500);
    vi.useRealTimers();

    await waitFor(() => expect(updateBody).not.toBeNull());
    expect(createBodies).toHaveLength(1);
  });
});
