import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";

vi.mock("@/components/query-editor/SqlEditor", () => ({
  SqlEditor: () => <textarea data-testid="sql-editor" />,
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

const BASE = "http://localhost:8080";
const DS = { id: 1, name: "Postgres", type: "pg", paused: false, pause_reason: null, schema_refreshed_at: null, created_at: "", updated_at: "" };
const QUERY = {
  id: 42, name: "Net revenue by store and month", query: "SELECT 1", data_source_id: 1,
  options: { parameters: [] }, is_archived: false,
  published: false, has_unpublished_changes: false, working_version: 1,
  visibility: "workspace",
  is_favorite: false, version: 1,
  description: "", user_id: 1, owner_name: "Ada Lovelace", latest_query_data_id: null,
  last_execution_status: "New" as const, last_execution_at: null,
  created_at: "", updated_at: "",
  caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
};

function renderEditor(path: string) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createMemoryRouter(
    [
      { path: "/queries/new", element: <QueryEditorPage /> },
      { path: "/queries/:id", element: <QueryEditorPage /> },
    ],
    { initialEntries: [path] },
  );
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("QueryEditorPage — document title and heading", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
    );
  });

  it("uses a section-noun title while a new query has no name yet", async () => {
    renderEditor("/queries/new");
    await screen.findByTestId("sql-editor");
    expect(document.title).toBe("New query · arcodash");
  });

  it("uses the query's own name once loaded, for both the tab title and the hidden heading", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));
    renderEditor("/queries/42");

    await waitFor(() => expect(document.title).toBe("Net revenue by store and month · arcodash"));
    expect(
      screen.getByRole("heading", { name: "Net revenue by store and month", hidden: true }),
    ).toBeInTheDocument();
  });

  it("falls back to the section noun while an existing query is still loading", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => new Promise(() => {})));
    renderEditor("/queries/42");
    await waitFor(() => expect(document.title).toBe("Query · arcodash"));
  });

  it("falls back to 'Untitled query' if a loaded query has an empty name", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json({ ...QUERY, name: "" })));
    renderEditor("/queries/42");
    await waitFor(() => expect(document.title).toBe("Untitled query · arcodash"));
  });
});
