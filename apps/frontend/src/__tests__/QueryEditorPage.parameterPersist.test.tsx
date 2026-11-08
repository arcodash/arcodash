/** Adding a parameter goes through autosave, so it can't race an in-flight autosave and fabricate a
 * conflict. */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
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
    isPollError: false,
    error: null,
    queryResultId: null,
  })),
}));

import { QueryEditorPage } from "@/pages/QueryEditorPage";

const BASE = "http://localhost:8080";

const DS = { id: 1, name: "Postgres", type: "pg", paused: false, pause_reason: null, schema_refreshed_at: null, created_at: "", updated_at: "" };
const QUERY = {
  id: 42, name: "My query", query: "SELECT {{ foo }}", data_source_id: 1,
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
    [{ path: "/queries/:id", element: <QueryEditorPage /> }],
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

async function clickAddParameter(name: string) {
  const configurator = await screen.findByTestId(`param-${name}`);
  await userEvent.click(within(configurator).getByRole("button", { name: "Add parameter" }));
}

describe("persistParameterDefinitions", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
    );
  });

  it("adding a parameter while an autosave PATCH is in flight sends exactly one further PATCH, carrying the version the in-flight save just wrote — no conflict", async () => {
    let patchCount = 0;
    const patchBodies: Record<string, unknown>[] = [];
    let resolveFirstPatch: (() => void) | null = null;
    let firstPatchStarted = false;

    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        patchCount += 1;
        const body = (await request.json()) as Record<string, unknown>;
        patchBodies.push(body);
        const version = body.expected_version as number;
        if (patchCount === 1) {
          firstPatchStarted = true;
          await new Promise<void>((resolve) => {
            resolveFirstPatch = resolve;
          });
        }
        return HttpResponse.json({
          ...QUERY,
          query: body.query ?? QUERY.query,
          options: body.options ?? QUERY.options,
          working_version: version + 1,
        });
      }),
    );

    renderEditor("/queries/42");
    await screen.findByTestId("param-foo");

    const editor = await screen.findByTestId("sql-editor");
    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT {{ foo }} -- edited" } });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1200);
    });
    vi.useRealTimers();

    await waitFor(() => expect(firstPatchStarted).toBe(true));
    expect(patchCount).toBe(1); // in flight, not yet resolved

    await clickAddParameter("foo");

    await act(async () => {
      resolveFirstPatch?.();
      await Promise.resolve();
    });

    await waitFor(() => expect(patchCount).toBe(2));
    // Second PATCH carries the version the first save wrote (2), not the stale snapshot (1), which
    // would 409.
    expect(patchBodies[1].expected_version).toBe(2);
    expect((patchBodies[1].options as { parameters: unknown[] }).parameters).toHaveLength(1);

    expect(screen.queryByText(/changed by someone else/)).not.toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByTestId("param-foo").querySelector("input")).toBeInTheDocument(),
    );
  });

  it("adding a parameter with no save in flight persists it via a single PATCH", async () => {
    let patchBody: Record<string, unknown> | null = null;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        patchBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({
          ...QUERY,
          options: patchBody.options ?? QUERY.options,
          working_version: 2,
        });
      }),
    );

    renderEditor("/queries/42");
    await clickAddParameter("foo");

    await waitFor(() => expect(patchBody).not.toBeNull());
    expect((patchBody as unknown as { options: { parameters: { name: string }[] } }).options.parameters).toEqual([
      expect.objectContaining({ name: "foo" }),
    ]);
    await waitFor(() =>
      expect(screen.getByTestId("param-foo").querySelector("input")).toBeInTheDocument(),
    );
  });

  it("adding a parameter on a brand-new (not yet created) query does not throw and does not POST — the definition rides the first real save", async () => {
    let createCalled = false;
    server.use(
      http.post(`${BASE}/api/queries`, () => {
        createCalled = true;
        return HttpResponse.json({ ...QUERY, id: 99 });
      }),
      http.get(`${BASE}/api/queries/99`, () => HttpResponse.json({ ...QUERY, id: 99, name: "My draft" })),
    );

    renderEditor("/queries/new");
    const editor = await screen.findByTestId("sql-editor");
    fireEvent.change(editor, { target: { value: "SELECT {{ foo }}" } });

    await clickAddParameter("foo");

    await new Promise((r) => setTimeout(r, 50));
    expect(createCalled).toBe(false);
    await waitFor(() =>
      expect(screen.getByTestId("param-foo").querySelector("input")).toBeInTheDocument(),
    );

    const nameInput = screen.getByDisplayValue("Untitled query");
    vi.useFakeTimers();
    fireEvent.change(nameInput, { target: { value: "My draft" } });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    vi.useRealTimers();

    await waitFor(() => expect(createCalled).toBe(true));
  });

  it("a real 500 on the parameter-save PATCH surfaces the failure instead of silently reporting success, through the actual useAutosave/QueryEditorPage wiring", async () => {
    let patchCount = 0;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        patchCount += 1;
        const body = (await request.json()) as Record<string, unknown>;
        if (patchCount === 1) {
          return HttpResponse.json({ message: "boom" }, { status: 500 });
        }
        return HttpResponse.json({
          ...QUERY,
          options: body.options ?? QUERY.options,
          working_version: 2,
        });
      }),
    );

    renderEditor("/queries/42");
    await clickAddParameter("foo");

    // Failure goes through the real PATCH and surfaces via the page's failure UI, not swallowed.
    await waitFor(() => expect(patchCount).toBe(1));
    expect(await screen.findByText("Save failed")).toBeInTheDocument();
    expect(await screen.findByText("Failed to save parameter")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    await waitFor(() => expect(patchCount).toBe(2));
    expect(await screen.findByText("Saved")).toBeInTheDocument();
    expect(screen.queryByText("Save failed")).not.toBeInTheDocument();
  });
});
