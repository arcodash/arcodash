import { describe, it, expect, vi, beforeEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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
  id: 5, name: "Revenue", query: "SELECT 1", data_source_id: 1,
  options: { parameters: [] }, is_archived: false,
  published: false, has_unpublished_changes: false, working_version: 1,
  visibility: "workspace",
  is_favorite: false, version: 1,
  description: "", user_id: 1, owner_name: "Ada Lovelace", latest_query_data_id: null,
  last_execution_status: "New" as const, last_execution_at: null,
  created_at: "", updated_at: "",
  verified_at: null as string | null, verified_by: null as number | null,
  caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: true },
};
const VERIFIED = { ...QUERY, verified_at: "2026-01-01T00:00:00Z", verified_by: 1 };

function renderEditor() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const router = createMemoryRouter(
    [{ path: "/queries/:id", element: <QueryEditorPage /> }],
    { initialEntries: ["/queries/5"] },
  );
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("QueryEditorPage verified toggle", () => {
  const calls: string[] = [];
  beforeEach(() => {
    isAdmin = true;
    calls.length = 0;
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/queries/5/verify`, () => {
        calls.push("verify");
        return HttpResponse.json(VERIFIED);
      }),
      http.delete(`${BASE}/api/queries/5/verify`, () => {
        calls.push("unverify");
        return HttpResponse.json(QUERY);
      }),
    );
  });

  it("lets an admin mark a query verified and flips the label", async () => {
    server.use(http.get(`${BASE}/api/queries/5`, () => HttpResponse.json(QUERY)));
    renderEditor();

    fireEvent.click(await screen.findByRole("button", { name: "Mark as verified" }));

    expect(await screen.findByRole("button", { name: "Verified · remove" })).toBeInTheDocument();
    expect(calls).toEqual(["verify"]);
  });

  it("hides the toggle from non-admins", async () => {
    isAdmin = false;
    server.use(http.get(`${BASE}/api/queries/5`, () => HttpResponse.json(QUERY)));
    renderEditor();

    await screen.findByTestId("sql-editor");
    await waitFor(() => expect(document.title).toBe("Revenue · arcodash"));
    expect(screen.queryByRole("button", { name: "Mark as verified" })).not.toBeInTheDocument();
  });

  it("removes verification when a verified query's button is clicked", async () => {
    server.use(http.get(`${BASE}/api/queries/5`, () => HttpResponse.json(VERIFIED)));
    renderEditor();

    fireEvent.click(await screen.findByRole("button", { name: "Verified · remove" }));

    expect(await screen.findByRole("button", { name: "Mark as verified" })).toBeInTheDocument();
    expect(calls).toEqual(["unverify"]);
  });
});
