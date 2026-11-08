/** Executing from the editor must invalidate the /queries list so client-side navigation back shows
 * fresh status. Both pages mount against one QueryClient (a real route transition hits a
 * jsdom/undici bug). */
import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router";
import { MemoryRouter } from "react-router";
import type * as ReactRouterModule from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { mockViewport } from "./mockViewport";
import { AuthProvider } from "@/lib/auth";
import { QueryEditorPage } from "@/pages/QueryEditorPage";
import { QueriesPage } from "@/pages/QueriesPage";

const navigateSpy = vi.fn();
vi.mock("react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof ReactRouterModule>();
  return { ...actual, useNavigate: () => navigateSpy };
});

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: vi.fn(({ value, onChange, className }) => (
    <textarea
      data-testid="sql-editor"
      className={className}
      value={value}
      onChange={(e) => onChange?.(e.target.value)}
    />
  )),
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

const BASE = "http://localhost:8080";

const QUERY = {
  id: 42, name: "My query", query: "SELECT 1", data_source_id: 1,
  options: { parameters: [] }, is_archived: false,
  published: true, has_unpublished_changes: false, working_version: 1,
  visibility: "workspace",
  is_favorite: false, version: 1,
  description: "", user_id: 1, owner_name: "Ada Lovelace", latest_query_data_id: null,
  last_execution_status: "Failed" as const, last_execution_at: "2026-08-01T00:00:00Z",
  created_at: "", updated_at: "",
  caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
};

function listRow(status: "Failed" | "Success", rowCount: number) {
  return {
    id: 42, name: "My query", description: "", data_source_id: 1, user_id: 1,
    published: true, has_unpublished_changes: false,
    last_execution_status: status, last_execution_at: "2026-08-01T00:00:00Z",
    // A Success row needs a result pointer or QueriesPage renders "Result unavailable".
    row_count: rowCount, latest_query_data_id: status === "Success" ? 1 : null,
    owner_name: "Ada Lovelace",
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
  };
}

function renderEditor(qc: QueryClient) {
  const router = createMemoryRouter(
    [{ path: "/queries/:id", element: <QueryEditorPage /> }],
    { initialEntries: ["/queries/42"] },
  );
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
}

function renderList(qc: QueryClient) {
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

describe("executing a query in the editor invalidates the queries list", () => {
  it("shows the fresh status/row-count on a fresh mount of QueriesPage sharing the same cache, not the stale pre-execution snapshot", async () => {
    const restoreViewport = mockViewport(true); // QueriesPage's table view needs a desktop matchMedia.

    let listStatus: "Failed" | "Success" = "Failed";
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.get(`${BASE}/api/queries`, () =>
        HttpResponse.json([listRow(listStatus, listStatus === "Success" ? 500 : 3)]),
      ),
      http.post(`${BASE}/api/query_results`, () => {
        listStatus = "Success";
        return HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: Array.from({ length: 500 }, (_, i) => ({ count: i })),
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: "",
          },
        });
      }),
    );

    // Must match the real staleTime (30_000); with 0, any remount refetches and the test passes
    // without the invalidation.
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 30_000 } } });

    try {
      // Visit the list first so its query is cached; otherwise there's nothing to invalidate.
      const list1 = renderList(qc);
      const staleRow = await screen.findByText("My query");
      expect(within(staleRow.closest("tr")!).getByText("Failed")).toBeInTheDocument();
      list1.unmount();

      const editor = renderEditor(qc);

      const executeBtn = await screen.findByRole("button", { name: /execute/i });
      await userEvent.click(executeBtn);
      await waitFor(() => {
        expect(screen.getByText(/500 rows/i)).toBeInTheDocument();
      });

      await userEvent.click(screen.getByRole("button", { name: "Back to queries" }));
      expect(navigateSpy).toHaveBeenCalledWith("/queries");

      // Simulate the route change: unmount editor, mount list, same QueryClient.
      editor.unmount();
      renderList(qc);

      const row = await screen.findByText("My query");
      const tableRow = row.closest("tr");
      expect(tableRow).not.toBeNull();
      // Within staleTime, so a refetch only happens if execute invalidated `["queries", ...]`.
      await waitFor(() => {
        expect(within(tableRow!).getByText("Success")).toBeInTheDocument();
      });
      expect(within(tableRow!).getByText("500")).toBeInTheDocument();
      expect(within(tableRow!).queryByText("Failed")).not.toBeInTheDocument();
    } finally {
      restoreViewport();
    }
  });
});
