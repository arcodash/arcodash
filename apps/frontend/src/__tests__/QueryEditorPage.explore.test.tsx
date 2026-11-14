import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";

let isAdmin = true;
vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: (k: string) => (k === "admin" ? isAdmin : true) }),
}));

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: ({ value }: { value: string }) => <textarea data-testid="sql-editor" value={value} readOnly />,
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
  usePollJob: () => ({
    isSuccess: false,
    isDone: false,
    isPending: false,
    isStarted: false,
    isFailure: false,
    isCancelled: false,
    error: null,
    queryResultId: null,
  }),
}));

import { QueryEditorPage } from "@/pages/QueryEditorPage";

import { exploreState } from "@/lib/explore";

const BASE = "http://localhost:8080";
const ds = (id: number, name: string, type: string, display: string) => ({
  id, name, type, type_display_name: display, paused: false, pause_reason: null,
  schema_refreshed_at: null, created_at: "", updated_at: "",
});

describe("QueryEditorPage explore prefill", () => {
  it("opens with the explore SQL, name and data source without creating a query", async () => {
    const posts: string[] = [];
    server.use(
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({
          data_sources: [ds(1, "Postgres", "pg", "PostgreSQL"), ds(9, "Query Results", "query_results", "Query Results")],
          can_create: true,
        }),
      ),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/queries`, () => {
        posts.push("create");
        return HttpResponse.json({}, { status: 500 });
      }),
    );
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const router = createMemoryRouter([{ path: "/queries/:id", element: <QueryEditorPage /> }], {
      initialEntries: [{ pathname: "/queries/new", state: exploreState(42, "Revenue by region", 9) }],
    });
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <RouterProvider router={router} />
        </AuthProvider>
      </QueryClientProvider>,
    );

    const editor = await screen.findByTestId("sql-editor");
    expect((editor as HTMLTextAreaElement).value).toContain("FROM query_42");
    expect(screen.getByLabelText("Query name")).toHaveValue("Explore: Revenue by region");
    await waitFor(() =>
      expect(screen.getByTestId("query-editor-data-source-select")).toHaveTextContent("Query Results"),
    );
    expect(screen.getByTestId("query-editor-data-source-select")).not.toHaveTextContent("Postgres");
    expect(posts).toEqual([]);
  });
});
