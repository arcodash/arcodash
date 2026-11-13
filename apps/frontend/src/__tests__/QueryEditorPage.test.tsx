import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router";
import type * as ReactRouterModule from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { extensions } from "@/extensions";
import { AuthProvider } from "@/lib/auth";
import { PERMISSION_KEYS } from "@/lib/permissions";

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
  VersionHistoryPanel: vi.fn(() => <div data-testid="version-history" />),
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
  VizPanel: (props: { autoCreateRequest?: unknown }) => (
    <div data-testid="viz-panel" data-auto={JSON.stringify(props.autoCreateRequest ?? null)} />
  ),
}));

// Mocked navigate: real route transitions break in jsdom/undici. `useParams` still calls the real
// hook (stable hook order); tests override its return via `setParamsOverride`.
const { navigateSpy, useParamsSpy, setActualUseParams, setParamsOverride } = vi.hoisted(() => {
  let actualUseParams: ((...args: unknown[]) => unknown) | null = null;
  let override: unknown = null;
  return {
    navigateSpy: vi.fn(),
    useParamsSpy: vi.fn((...args: unknown[]) => {
      const real = actualUseParams?.(...args);
      return override ?? real;
    }),
    setActualUseParams: (fn: (...args: unknown[]) => unknown) => {
      actualUseParams = fn;
    },
    setParamsOverride: (value: unknown) => {
      override = value;
    },
  };
});
vi.mock("react-router", async (importOriginal) => {
  const actual = await importOriginal<typeof ReactRouterModule>();
  setActualUseParams(actual.useParams as (...args: unknown[]) => unknown);
  return { ...actual, useNavigate: () => navigateSpy, useParams: useParamsSpy };
});

const DEFAULT_JOB_STATE = {
  isSuccess: false,
  isDone: false,
  isPending: false,
  isStarted: false,
  isFailure: false,
  isCancelled: false,
  isPollError: false,
  error: null as string | null,
  queryResultId: null as number | null,
};

vi.mock("@/lib/jobs", () => ({
  usePollJob: vi.fn(() => DEFAULT_JOB_STATE),
}));

import { QueryEditorPage } from "@/pages/QueryEditorPage";
import { usePollJob } from "@/lib/jobs";
import { VersionHistoryPanel } from "@/components/query-editor/VersionHistoryPanel";

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

function askSse(events: { type: string; [k: string]: unknown }[]) {
  const body = events
    .map(({ type, ...data }) => `event: ${type}\ndata: ${JSON.stringify(data)}\n\n`)
    .join("");
  return new HttpResponse(body, { headers: { "Content-Type": "text/event-stream" } });
}

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

/** Fires one `change` event per character, like a fast typist. */
function typeCharsInto(element: HTMLElement, text: string) {
  let acc = "";
  for (const ch of text) {
    acc += ch;
    fireEvent.change(element, { target: { value: acc } });
  }
}

function renderEditor(path: string) {
  const qc = makeClient();
  // useBlocker needs a data router; one `queries/:id` route keeps a self-triggered create's
  // navigate on the same entry.
  const router = createMemoryRouter(
    [{ path: "/queries/:id", element: <QueryEditorPage /> }],
    { initialEntries: [path] },
  );
  const result = render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <RouterProvider router={router} />
      </AuthProvider>
    </QueryClientProvider>,
  );
  return { ...result, router, queryClient: qc };
}

// Default session has no `client_config`, so `aiEnabled` is false; override to exercise
// Fix-with-AI.
function enableAi() {
  server.use(
    http.get(`${BASE}/api/session`, () =>
      HttpResponse.json({
        user: {
          id: 1,
          name: "Test User",
          email: "test@example.com",
          groups: [1],
          org_id: 1,
          is_invitation_pending: false,
          disabled_at: null,
          profile_image_url: null,
          details: {},
          created_at: "2025-01-01T00:00:00Z",
          updated_at: "2025-01-01T00:00:00Z",
        },
        org: { id: 1, name: "Test Org", slug: "test-org", settings: {} },
        permissions: [...PERMISSION_KEYS],
        is_admin: true,
        client_config: {
          disable_public_urls: false,
          saml_enabled: false,
          ldap_enabled: false,
          ai_sql_generation: true,
        },
      }),
    ),
  );
}

describe("QueryEditorPage", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
    );
  });

  it("renders the SQL editor on new-query route", async () => {
    renderEditor("/queries/new");
    expect(await screen.findByTestId("sql-editor")).toBeInTheDocument();
  });

  it("shows parameters panel when query has params", async () => {
    const queryWithParam = {
      ...QUERY,
      query: "SELECT {{ name }}",
      options: {
        parameters: [{ name: "name", title: "Name", type: "text" as const, value: "" }],
      },
    };
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(queryWithParam)),
    );

    renderEditor("/queries/42");

    await waitFor(() => {
      expect(screen.getByDisplayValue("SELECT {{ name }}")).toBeInTheDocument();
    });
  });

  it("shows the page loader while the existing query is still loading", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => new Promise(() => {})),
    );

    renderEditor("/queries/42");

    expect(await screen.findByRole("status")).toBeInTheDocument();
    expect(screen.queryByTestId("sql-editor")).not.toBeInTheDocument();
  });

  it("renders the saved-query action bar with wide buttons queryable by role+name, and mounts the overflow menu only when opened", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));

    renderEditor("/queries/42");

    // jsdom renders both the wide buttons and the overflow trigger; the hide/show split is CSS.
    expect(await screen.findByRole("button", { name: "Execute" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fork" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Publish" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Archive" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "History" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Star" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Schedule/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Refresh now" })).not.toBeInTheDocument();

    expect(screen.queryByRole("menu")).not.toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "More actions" }));
    const menu = await screen.findByRole("menu");
    expect(menu).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: /^Schedule/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Refresh now" })).not.toBeInTheDocument();
  });

  it("Publish shows no 'never run successfully' warning for a query with a successful run", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({
          ...QUERY,
          last_execution_status: "Success",
          last_execution_at: new Date().toISOString(),
          row_count: 221,
          latest_query_data_id: 246,
        }),
      ),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Publish" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).queryByText(/haven't run this query successfully yet/i),
    ).not.toBeInTheDocument();
  });

  it("Publish still shows the warning for a query that has never run successfully", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, last_execution_status: "New", last_execution_at: null }),
      ),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Publish" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).getByText(/haven't run this query successfully yet/i),
    ).toBeInTheDocument();
  });

  it("a successful Execute refreshes the cached query so Publish stops warning without a reload", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, last_execution_status: "New", last_execution_at: null }),
      ),
      http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            query: "SELECT 1",
            data: { columns: [], rows: [] },
            runtime: 0.01,
            retrieved_at: new Date().toISOString(),
          },
        }),
      ),
    );
    let refetchCount = 0;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => {
        refetchCount += 1;
        return HttpResponse.json(
          refetchCount === 1
            ? { ...QUERY, last_execution_status: "New", last_execution_at: null }
            : { ...QUERY, last_execution_status: "Success", last_execution_at: new Date().toISOString() },
        );
      }),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Execute" }));
    await waitFor(() => expect(refetchCount).toBeGreaterThanOrEqual(2));

    await userEvent.click(await screen.findByRole("button", { name: "Publish" }));
    const dialog = await screen.findByRole("dialog");
    expect(
      within(dialog).queryByText(/haven't run this query successfully yet/i),
    ).not.toBeInTheDocument();
  });

  it("clicking Publish and confirming fires POST /queries/42/publish", async () => {
    let published = false;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42/publish`, () => {
        published = true;
        return HttpResponse.json({ ...QUERY, published: true, has_unpublished_changes: false });
      }),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Publish" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Publish" }));

    await waitFor(() => expect(published).toBe(true));
  });

  it("autosaving an edit to an existing query fires the update endpoint", async () => {
    let updateBody: Record<string, unknown> | null = null;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        updateBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ ...QUERY, query: updateBody.query as string, working_version: 2 });
      }),
    );

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    vi.useRealTimers();

    await waitFor(() => expect(updateBody).not.toBeNull());
    expect((updateBody as unknown as Record<string, unknown>).query).toBe("SELECT * FROM widgets");
  });

  it("mounts the freshness chip and the schedule section for a saved query", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));

    renderEditor("/queries/42");

    expect(await screen.findByTestId("freshness-chip")).toBeInTheDocument();
    expect(screen.getByTestId("schedule-section")).toBeInTheDocument();
  });

  it("shows the owner avatar with a structured 'Owner' tooltip card on hover, distinct from the info icon's tooltip", async () => {
    const queryWithHistory = {
      ...QUERY,
      last_execution_status: "Success" as const,
      last_execution_at: "2026-06-01T12:00:00Z",
      created_at: "2026-05-01T09:00:00Z",
      updated_at: "2026-06-15T14:30:00Z",
    };
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(queryWithHistory)),
    );

    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });

    const lastRunTime = new Date("2026-06-01T12:00:00Z").toLocaleString();
    const createdAt = new Date("2026-05-01T09:00:00Z").toLocaleString();
    const updatedAt = new Date("2026-06-15T14:30:00Z").toLocaleString();

    const ownerBtn = screen.getByRole("button", { name: "Owner: Ada Lovelace" });
    expect(ownerBtn).toHaveTextContent("AL"); // initials fallback

    expect(screen.queryByRole("tooltip")).not.toBeInTheDocument();

    const user = userEvent.setup();
    await user.hover(ownerBtn);
    const ownerTooltip = await screen.findByRole("tooltip");
    expect(within(ownerTooltip).getByText("Owner")).toBeInTheDocument();
    expect(within(ownerTooltip).getByText("Ada Lovelace")).toBeInTheDocument();
    await user.unhover(ownerBtn);

    const infoBtn = screen.getByRole("button", { name: "Query details" });
    await user.hover(infoBtn);
    const infoTooltip = await screen.findByRole("tooltip");
    expect(within(infoTooltip).getByText("Query info")).toBeInTheDocument();
    expect(within(infoTooltip).getByText("Last run")).toBeInTheDocument();
    expect(infoTooltip).toHaveTextContent(`Success, ${lastRunTime}`);
    expect(within(infoTooltip).getByText("Created")).toBeInTheDocument();
    expect(infoTooltip).toHaveTextContent(createdAt);
    expect(within(infoTooltip).getByText("Updated")).toBeInTheDocument();
    expect(infoTooltip).toHaveTextContent(updatedAt);
    expect(infoTooltip).not.toHaveTextContent("Ada Lovelace");
    await user.unhover(infoBtn);

    fireEvent.focus(ownerBtn);
    const focusedTooltip = await screen.findByRole("tooltip");
    expect(within(focusedTooltip).getByText("Owner")).toBeInTheDocument();
    expect(within(focusedTooltip).getByText("Ada Lovelace")).toBeInTheDocument();
  });

  it("shows 'Never' in the info tooltip when the query has not been executed", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));
    renderEditor("/queries/42");
    const infoBtn = await screen.findByRole("button", { name: "Query details" });
    await userEvent.hover(infoBtn);
    const infoTooltip = await screen.findByRole("tooltip");
    expect(within(infoTooltip).getByText("Last run")).toBeInTheDocument();
    expect(infoTooltip).toHaveTextContent("Never");
  });

  it("back button navigates to the queries list", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));
    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });
    await userEvent.click(screen.getByRole("button", { name: "Back to queries" }));
    expect(navigateSpy).toHaveBeenCalledWith("/queries");
  });

  it("shows the honest draft-privacy hint and opens ShareDialog from the Share action", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.get(`${BASE}/api/queries/42/acl`, () => HttpResponse.json([])),
    );

    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });

    expect(
      screen.getByText(
        "Only you and people you've shared with can see this. Publishing makes it visible to your workspace.",
      ),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /learn more about drafts and published versions/i }),
    ).toHaveAttribute(
      "href",
      "https://www.arcodash.com/docs/queries-visualization/versioning-drafts#draft-vs-published",
    );

    await userEvent.click(screen.getByRole("button", { name: "Share" }));
    expect(await screen.findByText("People & groups")).toBeInTheDocument();
  });

  it("displays results table after execution returns inline result", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      // Execute always runs live SQL ad hoc (POST /api/query_results), never /queries/:id/results.
      http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: [{ count: 1 }],
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: "",
          },
        }),
      ),
    );

    renderEditor("/queries/42");

    const executeBtn = await screen.findByRole("button", { name: /execute/i });
    await userEvent.click(executeBtn);

    await waitFor(() => {
      expect(screen.getByText(/1 row/i)).toBeInTheDocument();
    });
  });

  it("shows the DS-access-denied banner instead of a generic error when execute fails with that error_code", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json(
          { message: "no data source access", error_code: "data_source_access_required" },
          { status: 403 },
        ),
      ),
    );

    renderEditor("/queries/42");

    const executeBtn = await screen.findByRole("button", { name: /execute/i });
    await userEvent.click(executeBtn);

    expect(
      await screen.findByText(/you don't have access to its data source, so it can't run/i),
    ).toBeInTheDocument();
    expect(screen.queryByText("Run the query to see results.")).not.toBeInTheDocument();
  });

  it("loads and shows the query's already-stored result on mount, labeled with its age, without executing", async () => {
    const executeSpy = vi.fn();
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json({ ...QUERY, latest_query_data_id: 99 })),
      http.get(`${BASE}/api/queries/42/results`, () =>
        HttpResponse.json({
          query_result: {
            id: 99,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: [{ count: 1 }],
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: new Date(Date.now() - 9 * 60_000).toISOString(),
          },
        }),
      ),
      http.post(`${BASE}/api/query_results`, () => {
        executeSpy();
        return HttpResponse.json({ job: { id: "job-1", status: 1 } });
      }),
    );

    renderEditor("/queries/42");

    expect(await screen.findByText(/1 row/i)).toBeInTheDocument();
    expect(screen.getByText(/Updated 9 min ago/i)).toBeInTheDocument();
    expect(screen.queryByText("Run the query to see results.")).not.toBeInTheDocument();
    expect(executeSpy).not.toHaveBeenCalled();
  });

  it("still shows the stored result once the metadata fetch (which gates it) resolves", async () => {
    // Results fetch is gated on metadata, so it resolves after the hydrate effect clears `result`;
    // populate must re-apply it.
    server.use(
      http.get(`${BASE}/api/queries/42`, async () => {
        await new Promise((resolve) => setTimeout(resolve, 20));
        return HttpResponse.json({ ...QUERY, latest_query_data_id: 99 });
      }),
      http.get(`${BASE}/api/queries/42/results`, () =>
        HttpResponse.json({
          query_result: {
            id: 99,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: [{ count: 1 }],
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: new Date(Date.now() - 9 * 60_000).toISOString(),
          },
        }),
      ),
    );

    renderEditor("/queries/42");

    await screen.findByDisplayValue(QUERY.name);
    expect(await screen.findByText(/1 row/i)).toBeInTheDocument();
    expect(screen.getByText(/Updated 9 min ago/i)).toBeInTheDocument();
    expect(screen.queryByText("Run the query to see results.")).not.toBeInTheDocument();
  });

  it("shows the empty-results prompt, not an error, when the query has never produced a stored result", async () => {
    const resultsSpy = vi.fn();
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, latest_query_data_id: null }),
      ),
      http.get(`${BASE}/api/queries/42/results`, () => {
        resultsSpy();
        return HttpResponse.json({ message: "no results yet for this query" }, { status: 404 });
      }),
    );

    renderEditor("/queries/42");

    expect(await screen.findByText("Run the query to see results.")).toBeInTheDocument();
    expect(resultsSpy).not.toHaveBeenCalled();
  });

  it("names the saved visualization(s) instead of a bare run-me prompt for a never-run query that already owns one (e.g. a fresh fork)", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ ...QUERY, latest_query_data_id: null }),
      ),
      http.get(`${BASE}/api/queries/42/visualizations`, () =>
        HttpResponse.json([
          {
            id: 9,
            query_id: 42,
            type: "chart",
            name: "Revenue chart",
            description: null,
            options: {},
            created_at: "",
            updated_at: "",
          },
        ]),
      ),
    );

    renderEditor("/queries/42");

    expect(
      await screen.findByText(/This query has 1 saved visualization — Revenue chart\. Run the query to see them\./),
    ).toBeInTheDocument();
    expect(screen.queryByText("Run the query to see results.")).not.toBeInTheDocument();
  });

  it("hides the Share action when the caller's object_role is editor, not owner, even though can_edit is true", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({
          ...QUERY,
          caller: { object_role: "editor", can_edit: true, can_delete: false },
        }),
      ),
      http.get(`${BASE}/api/queries/42/acl`, () => HttpResponse.json([])),
    );

    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });

    expect(screen.getByRole("button", { name: "Publish" })).toBeInTheDocument();
    // Share is owner-tier only.
    expect(screen.queryByRole("button", { name: "Share" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "More actions" }));
    expect(await screen.findByRole("menu")).toBeInTheDocument();
    expect(screen.queryByRole("menuitem", { name: "Share" })).not.toBeInTheDocument();
  });

  it("renders the SQL editor read-only with an explanatory note for a caller without edit permission", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({
          ...QUERY,
          caller: { object_role: "viewer", can_edit: false, can_delete: false, can_execute: true },
        }),
      ),
    );

    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });

    expect(screen.getByTestId("sql-editor")).toBeDisabled();
    expect(
      screen.getByText("You don't have permission to edit this query. Viewing only."),
    ).toBeInTheDocument();
  });

  it("keeps the SQL editor editable, with no read-only note, for a caller with edit permission", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));

    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });

    expect(screen.getByTestId("sql-editor")).not.toBeDisabled();
    expect(
      screen.queryByText("You don't have permission to edit this query. Viewing only."),
    ).not.toBeInTheDocument();
  });

  it("keeps displayed results after a star-toggle refetches the same query", async () => {
    // Same query, only `is_favorite` flips: a refetch yields a new object reference.
    let favorite = false;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json({ ...QUERY, is_favorite: favorite })),
      http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: [{ count: 1 }],
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: "",
          },
        }),
      ),
      http.post(`${BASE}/api/queries/42/favorite`, () => {
        favorite = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );

    renderEditor("/queries/42");

    const executeBtn = await screen.findByRole("button", { name: /execute/i });
    await userEvent.click(executeBtn);
    await waitFor(() => {
      expect(screen.getByText(/1 row/i)).toBeInTheDocument();
    });

    await userEvent.click(screen.getByRole("button", { name: "Star" }));

    await waitFor(() => {
      expect(screen.getByRole("button", { name: "Unstar" })).toBeInTheDocument();
    });
    expect(screen.getByText(/1 row/i)).toBeInTheDocument();
  });

  it("shows an error message when starring fails, instead of silently snapping the icon back", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42/favorite`, () =>
        HttpResponse.json({ message: "Failed to star query" }, { status: 500 }),
      ),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Star" }));

    expect(await screen.findByText("Failed to star query")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Star" })).toBeInTheDocument();
  });

  it("disables Execute and shows the capability banner for a saved query blocked by capability", async () => {
    const blocked = {
      ...QUERY,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: false, execute_blocked_reason: "capability" as const },
    };
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(blocked)));

    renderEditor("/queries/42");

    const execBtn = await screen.findByRole("button", { name: "Execute" });
    expect(execBtn).toBeDisabled();
    expect(execBtn).toHaveAttribute(
      "title",
      "Your current role doesn't allow running queries. Ask an admin to check your roles and permissions.",
    );
    expect(
      screen.getByText(
        "Your current role doesn't allow running queries. Ask an admin to check your roles and permissions.",
      ),
    ).toBeInTheDocument();
  });

  it("shows the data_source banner when a saved query is blocked by data-source access", async () => {
    const blocked = {
      ...QUERY,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: false, execute_blocked_reason: "data_source" as const },
    };
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(blocked)));

    renderEditor("/queries/42");
    expect(await screen.findByRole("button", { name: "Execute" })).toBeDisabled();
    expect(
      screen.getByText(
        "You can open this query, but you don't have access to its data source, so it can't run. Ask an admin for access to the data source.",
      ),
    ).toBeInTheDocument();
  });

  it("shows the combined banner when a saved query is blocked by both", async () => {
    const blocked = {
      ...QUERY,
      caller: { object_role: "owner", can_edit: true, can_delete: true, can_execute: false, execute_blocked_reason: "both" as const },
    };
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(blocked)));

    renderEditor("/queries/42");
    expect(await screen.findByRole("button", { name: "Execute" })).toBeDisabled();
    expect(
      screen.getByText(
        "You have limited access to this query and its data source, so it can't run. Ask an admin to check your roles and permissions.",
      ),
    ).toBeInTheDocument();
  });

  it("keeps Execute enabled with no banner for a new (unsaved) query via the capability fallback", async () => {
    // A new query has no caller yet; must fall back to the capability check, not disable Execute.
    renderEditor("/queries/new");
    const execBtn = await screen.findByRole("button", { name: "Execute" });
    await waitFor(() => expect(execBtn).not.toBeDisabled());
    expect(execBtn).not.toHaveAttribute("title");
    expect(screen.queryByText(/Ask an admin/)).not.toBeInTheDocument();
  });

  it("defaults a new query's name to 'Untitled query' so it's persistable without typing a name", async () => {
    renderEditor("/queries/new");
    expect(await screen.findByDisplayValue("Untitled query")).toBeInTheDocument();
  });

  describe("new-query autosave gate", () => {
    it("explains its state on /queries/new before anything is typed", async () => {
      renderEditor("/queries/new");
      expect(
        await screen.findByText("Add SQL and a name — or run it — to save this query."),
      ).toBeInTheDocument();
    });

    it("does not create a query row from a name change alone — untouched scaffold SQL blocks it", async () => {
      let createCalled = false;
      server.use(
        http.post(`${BASE}/api/queries`, () => {
          createCalled = true;
          return HttpResponse.json({ ...QUERY, id: 99 });
        }),
      );

      renderEditor("/queries/new");
      const nameInput = await screen.findByDisplayValue("Untitled query");

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "My draft" } });
      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => {
        expect(createCalled).toBe(false);
      });
      expect(
        screen.getByText("Add SQL and a name — or run it — to save this query."),
      ).toBeInTheDocument();
    });

    it("does not create a query row from a SQL change alone — untouched name blocks it", async () => {
      let createCalled = false;
      server.use(
        http.post(`${BASE}/api/queries`, () => {
          createCalled = true;
          return HttpResponse.json({ ...QUERY, id: 99 });
        }),
      );

      const { router } = renderEditor("/queries/new");
      const editor = await screen.findByTestId("sql-editor");

      vi.useFakeTimers();
      fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => {
        expect(createCalled).toBe(false);
      });
      expect(router.state.location.pathname).toBe("/queries/new");
      expect(
        screen.getByText("Add SQL and a name — or run it — to save this query."),
      ).toBeInTheDocument();
    });

    it("creates the row once both a name and real SQL are present, and swaps to the Saved indicator", async () => {
      let body: Record<string, unknown> | null = null;
      server.use(
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          body = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({ ...QUERY, id: 99, name: body.name, query: body.query });
        }),
      );

      renderEditor("/queries/new");
      const nameInput = await screen.findByDisplayValue("Untitled query");
      const editor = await screen.findByTestId("sql-editor");

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "My draft" } });
      fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => expect(body).not.toBeNull());
      expect((body as unknown as Record<string, unknown>).name).toBe("My draft");
      expect((body as unknown as Record<string, unknown>).query).toBe("SELECT * FROM widgets");
    });

    it("creates the row on the first Execute even with untouched scaffold SQL", async () => {
      let body: Record<string, unknown> | null = null;
      let execBody: Record<string, unknown> | null = null;
      server.use(
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          body = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({ ...QUERY, id: 99, name: body.name, query: body.query });
        }),
        http.post(`${BASE}/api/query_results`, async ({ request }) => {
          execBody = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({ query_result: { id: 1, query: "SELECT 1", data: { columns: [], rows: [] } } });
        }),
      );

      renderEditor("/queries/new");
      const execBtn = await screen.findByRole("button", { name: "Execute" });
      await waitFor(() => expect(execBtn).not.toBeDisabled());
      await userEvent.click(execBtn);

      await waitFor(() => expect(body).not.toBeNull());
      expect((body as unknown as Record<string, unknown>).query).toBe("SELECT 1");
      // Run must wait for the just-created row and bind its real id, not the stale null `queryId`.
      await waitFor(() => expect(execBody).not.toBeNull());
      expect((execBody as unknown as Record<string, unknown>).query_id).toBe(99);
    });

    it("first Execute on a real name+SQL new query (canPersist already true) still binds — exactly one create, execute body carries the created id", async () => {
      const createBodies: Record<string, unknown>[] = [];
      let execBody: Record<string, unknown> | null = null;
      server.use(
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          const body = (await request.json()) as Record<string, unknown>;
          createBodies.push(body);
          return HttpResponse.json({ ...QUERY, id: 123, name: body.name, query: body.query });
        }),
        http.post(`${BASE}/api/query_results`, async ({ request }) => {
          execBody = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({ query_result: { id: 1, query: "SELECT * FROM widgets", data: { columns: [], rows: [] } } });
        }),
      );

      renderEditor("/queries/new");
      const nameInput = await screen.findByDisplayValue("Untitled query");
      const editor = await screen.findByTestId("sql-editor");
      const execBtn = await screen.findByRole("button", { name: "Execute" });
      await waitFor(() => expect(execBtn).not.toBeDisabled());

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "Real query" } });
      fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
      // Click Execute before the ~1s autosave debounce fires.
      fireEvent.click(execBtn);

      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => expect(execBody).not.toBeNull());
      await waitFor(() => expect(createBodies.length).toBeGreaterThan(0));
      // Trailing settle window in case a second create is in flight.
      await new Promise((r) => setTimeout(r, 50));

      expect(createBodies).toHaveLength(1);
      expect((execBody as unknown as Record<string, unknown>).query_id).toBe(123);
    });
  });

  describe("fast-typing create race", () => {
    it("a full multi-line SQL statement typed keystroke-by-keystroke survives an immediate Execute click, never truncated to its first character", async () => {
      // Record every create, not just the last, so a truncated eager create can't hide.
      const createBodies: Record<string, unknown>[] = [];
      let execBody: Record<string, unknown> | null = null;
      server.use(
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          const body = (await request.json()) as Record<string, unknown>;
          createBodies.push(body);
          return HttpResponse.json({ ...QUERY, id: 99, name: body.name, query: body.query });
        }),
        http.post(`${BASE}/api/query_results`, async ({ request }) => {
          execBody = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({
            query_result: { id: 1, query: execBody.query, data: { columns: [], rows: [] } },
          });
        }),
      );

      renderEditor("/queries/new");
      const nameInput = await screen.findByDisplayValue("Untitled query");
      const editor = await screen.findByTestId("sql-editor");
      const execBtn = await screen.findByRole("button", { name: "Execute" });
      await waitFor(() => expect(execBtn).not.toBeDisabled());

      const fullSql = "SELECT id, name\nFROM widgets\nWHERE active = true";

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "Widgets report" } });
      typeCharsInto(editor, fullSql);
      // Click Execute before the autosave debounce fires.
      fireEvent.click(execBtn);

      expect(editor).toHaveValue(fullSql);

      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => expect(execBody).not.toBeNull());
      expect((execBody as unknown as Record<string, unknown>).query).toBe(fullSql);

      // Autosave creates the row exactly once, with the full text.
      await waitFor(() => expect(createBodies.length).toBeGreaterThan(0));
      await new Promise((r) => setTimeout(r, 50));
      expect(createBodies).toHaveLength(1);
      expect(createBodies[0].query).toBe(fullSql);
      expect(createBodies[0].name).toBe("Widgets report");
    });

    it("a name typed immediately after applying AI-generated SQL is never truncated to its first character", async () => {
      enableAi();
      const createBodies: Record<string, unknown>[] = [];
      server.use(
        http.post(`${BASE}/api/queries/ask`, () => askSse([{ type: "final", sql: "SELECT count(*) FROM orders WHERE status = 'paid'", explanation: null, served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1 }])),
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          const body = (await request.json()) as Record<string, unknown>;
          createBodies.push(body);
          return HttpResponse.json({ ...QUERY, id: 77, name: body.name, query: body.query });
        }),
      );

      renderEditor("/queries/new");
      await userEvent.click(await screen.findByRole("button", { name: /ask with ai/i }));
      await userEvent.type(screen.getByLabelText(/ask ai a question about your data/i), "count paid orders");
      await userEvent.click(screen.getByRole("button", { name: "Generate SQL" }));
      await userEvent.click(await screen.findByRole("button", { name: "Apply" }));

      const nameInput = await screen.findByLabelText(/query name/i);
      const fullName = "Paid orders count";

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "" } });
      typeCharsInto(nameInput, fullName);
      expect(nameInput).toHaveValue(fullName);

      act(() => {
        vi.advanceTimersByTime(2000);
      });
      vi.useRealTimers();

      await waitFor(() => expect(createBodies.length).toBeGreaterThan(0));
      await new Promise((r) => setTimeout(r, 50));
      expect(createBodies).toHaveLength(1);
      expect(createBodies[0].name).toBe(fullName);
      expect(createBodies[0].query).toBe("SELECT count(*) FROM orders WHERE status = 'paid'");
    });
  });

  describe("AI request unmount abort", () => {
    it("unmounting the editor aborts an in-flight AI request", async () => {
      enableAi();
      server.use(
        // Never resolves — keeps the mutation pending so its controller is still live at unmount.
        http.post(`${BASE}/api/queries/ask`, () => new Promise(() => {})),
      );
      const abortSpy = vi.spyOn(AbortController.prototype, "abort");

      const { unmount } = renderEditor("/queries/new");
      await userEvent.click(await screen.findByRole("button", { name: /ask with ai/i }));
      await userEvent.type(
        screen.getByLabelText(/ask ai a question about your data/i),
        "count paid orders",
      );
      await userEvent.click(screen.getByRole("button", { name: "Generate SQL" }));

      // Assert on the delta unmount causes: React Query aborts unrelated controllers during normal
      // render.
      await screen.findByRole("button", { name: "Cancel" });
      const callsBeforeUnmount = abortSpy.mock.calls.length;

      unmount();

      expect(abortSpy.mock.calls.length).toBeGreaterThan(callsBeforeUnmount);
    });
  });

  describe("create-time route swap", () => {
    afterEach(() => {
      // Undo the useParams override so it doesn't leak into other tests.
      setParamsOverride(null);
    });

    it("skips re-hydrating from the id a self-triggered create just produced, so a same-instance edit made right after isn't clobbered by the fetched copy", async () => {
      let createBody: Record<string, unknown> | null = null;
      server.use(
        http.post(`${BASE}/api/queries`, async ({ request }) => {
          createBody = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({
            ...QUERY,
            id: 123,
            name: createBody.name as string,
            query: createBody.query as string,
            working_version: 1,
          });
        }),
        http.get(`${BASE}/api/queries/123`, () =>
          HttpResponse.json({ ...QUERY, id: 123, name: "My draft", query: "SELECT * FROM widgets", working_version: 1 }),
        ),
        http.post(`${BASE}/api/queries/123`, async ({ request }) => {
          const body = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({ ...QUERY, id: 123, name: body.name as string, query: body.query as string, working_version: 2 });
        }),
      );

      renderEditor("/queries/new");
      const nameInput = await screen.findByDisplayValue("Untitled query");
      const editor = await screen.findByTestId("sql-editor");

      vi.useFakeTimers();
      fireEvent.change(nameInput, { target: { value: "My draft" } });
      fireEvent.change(editor, { target: { value: "SELECT * FROM widgets" } });
      act(() => {
        vi.advanceTimersByTime(1500);
      });
      vi.useRealTimers();

      // Cache seed and self-created-id ref are set before `navigate` (mocked no-op) is called.
      await waitFor(() => expect(createBody).not.toBeNull());

      // This edit must not be reverted by the id-driven hydrate.
      fireEvent.change(screen.getByTestId("sql-editor"), {
        target: { value: "SELECT * FROM widgets ORDER BY id" },
      });

      // Simulate `:id` becoming 123 without remount; a distinct state update makes React call
      // `useParams()` again.
      setParamsOverride({ id: "123" });
      fireEvent.change(screen.getByPlaceholderText("Add a description…"), {
        target: { value: "trigger a re-render" },
      });

      await waitFor(() =>
        expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT * FROM widgets ORDER BY id"),
      );
      expect(screen.getByDisplayValue("My draft")).toBeInTheDocument();
    });
  });

  it("keeps Snippets off the tab strip's own row below the sm breakpoint, instead of overlapping it", async () => {
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));
    renderEditor("/queries/42");

    const snippetMenu = await screen.findByTestId("snippet-menu");
    // `sm:contents` collapses the wrapper on desktop; below it Snippets right-aligns on its own
    // line.
    expect(snippetMenu.parentElement).toHaveClass("sm:contents");
    expect(snippetMenu.parentElement).toHaveClass("justify-end");

    const tabRow = snippetMenu.parentElement?.parentElement;
    expect(tabRow).toHaveClass("flex-col");
    expect(tabRow).toHaveClass("sm:flex-row");
  });

  it("shows a not-found state with a way back on a real 404", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ message: "Query not found" }, { status: 404 }),
      ),
    );

    renderEditor("/queries/42");

    expect(await screen.findByText("This query doesn't exist.")).toBeInTheDocument();
    expect(screen.getAllByText("Query not found")).toHaveLength(1);
    expect(screen.queryByTestId("sql-editor")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Back to queries" })).toBeInTheDocument();
  });

  it("shows a distinct failed-to-load state (not 'Query not found') on a non-404 error", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () =>
        HttpResponse.json({ message: "Internal server error" }, { status: 500 }),
      ),
    );

    renderEditor("/queries/42");

    expect(await screen.findByText("Failed to load query")).toBeInTheDocument();
    expect(await screen.findByText("Internal server error")).toBeInTheDocument();
    expect(screen.queryByText("Query not found")).not.toBeInTheDocument();
    expect(screen.queryByTestId("sql-editor")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Back to queries" })).toBeInTheDocument();
  });

  describe("Fix with AI", () => {
    afterEach(() => {
      vi.mocked(usePollJob).mockReturnValue(DEFAULT_JOB_STATE as unknown as ReturnType<typeof usePollJob>);
    });

    function failingQueryResults() {
      return http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({ message: 'syntax error at or near "SELEC"' }, { status: 400 }),
      );
    }

    async function renderIntoErrorState() {
      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: /execute/i }));
      await screen.findByText(/syntax error at or near/);
    }

    it("hides the trigger when the AI gate is off", async () => {
      server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)), failingQueryResults());

      await renderIntoErrorState();

      expect(screen.queryByRole("button", { name: /Fix with AI/ })).not.toBeInTheDocument();
    });

    it("shows the trigger and privacy caption when the AI gate is on", async () => {
      enableAi();
      server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)), failingQueryResults());

      await renderIntoErrorState();

      expect(await screen.findByRole("button", { name: /Fix with AI/ })).toBeInTheDocument();
      expect(screen.getByText(/never your query results/)).toBeInTheDocument();
    });

    it("banner click switches to Ask, prefills the question and streams with existing_sql + error", async () => {
      enableAi();
      let fixBody: Record<string, unknown> | null = null;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        failingQueryResults(),
        http.post(`${BASE}/api/queries/ask`, async ({ request }) => {
          fixBody = (await request.json()) as Record<string, unknown>;
          return askSse([{ type: "final", sql: "SELECT 1 AS ok", explanation: "You typed SELEC.", served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null }]);
        }),
      );

      await renderIntoErrorState();
      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));

      const box = await screen.findByLabelText(/ask ai a question about your data/i);
      expect(box).toHaveValue('Fix: syntax error at or near "SELEC"');
      expect(await screen.findByText("You typed SELEC.")).toBeInTheDocument();
      expect(fixBody).toEqual({
        data_source_id: 1,
        question: 'Fix: syntax error at or near "SELEC"',
        existing_sql: "SELECT 1",
        error: 'syntax error at or near "SELEC"',
      });
    });

    it("shows the Fixing link while the stream runs and it returns to the Ask tab", async () => {
      enableAi();
      let resolveFix!: (r: Response) => void;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        failingQueryResults(),
        http.post(
          `${BASE}/api/queries/ask`,
          () => new Promise<Response>((resolve) => { resolveFix = resolve; }),
        ),
      );

      await renderIntoErrorState();
      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));
      await screen.findByLabelText(/ask ai a question about your data/i);
      await userEvent.click(screen.getByRole("button", { name: "SQL Editor" }));
      expect(screen.queryByLabelText(/ask ai a question about your data/i)).not.toBeInTheDocument();
      expect(screen.queryByRole("button", { name: "✨ Fix with AI" })).not.toBeInTheDocument();

      await userEvent.click(screen.getByRole("button", { name: "Fixing in Ask with AI →" }));
      expect(screen.getByLabelText(/ask ai a question about your data/i)).toBeInTheDocument();

      resolveFix(askSse([{ type: "final", sql: "SELECT 1 AS ok", explanation: "You typed SELEC.", served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null }]));
      await waitFor(() =>
        expect(screen.queryByRole("button", { name: "Fixing in Ask with AI →" })).not.toBeInTheDocument(),
      );
      expect(screen.getByRole("button", { name: "✨ Fix with AI" })).toBeInTheDocument();
    });

    it("shows the trigger for an async job FAILURE (not just a synchronous 400) and sends the job's error", async () => {
      enableAi();
      let fixBody: Record<string, unknown> | null = null;
      vi.mocked(usePollJob).mockReturnValue({
        ...DEFAULT_JOB_STATE,
        isDone: true,
        isFailure: true,
        error: 'column "nope" does not exist',
      } as unknown as ReturnType<typeof usePollJob>);
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "failure" } }),
        ),
        http.post(`${BASE}/api/queries/ask`, async ({ request }) => {
          fixBody = (await request.json()) as Record<string, unknown>;
          return askSse([{ type: "final", sql: "SELECT 1 AS ok", explanation: "You typed SELEC.", served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null }]);
        }),
      );

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: /execute/i }));
      await screen.findByText('column "nope" does not exist');

      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));

      await waitFor(() => expect(fixBody).not.toBeNull());
      expect(fixBody!.error).toBe('column "nope" does not exist');
      expect(fixBody!.question).toBe('Fix: column "nope" does not exist');
    });

    it("does NOT offer Fix with AI for a poll error (lost track of the job) — there's no SQL to fix", async () => {
      enableAi();
      vi.mocked(usePollJob).mockReturnValue({
        ...DEFAULT_JOB_STATE,
        isDone: true,
        isFailure: true,
        isPollError: true,
        error: "Lost track of this run — it may have finished without us seeing it. Try running it again.",
      } as unknown as ReturnType<typeof usePollJob>);
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "started" } }),
        ),
      );

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: /execute/i }));

      expect(await screen.findByText(/lost track of this run/i)).toBeInTheDocument();
      expect(screen.queryByRole("button", { name: /Fix with AI/ })).not.toBeInTheDocument();
    });

    it("Apply from the Ask tab puts the fix in the editor without executing", async () => {
      enableAi();
      let execCalls = 0;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () => {
          execCalls += 1;
          return HttpResponse.json({ message: 'syntax error at or near "SELEC"' }, { status: 400 });
        }),
        http.post(`${BASE}/api/queries/ask`, () => askSse([{ type: "final", sql: "SELECT 1 AS ok", explanation: "You typed SELEC.", served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null }])),
      );

      await renderIntoErrorState();
      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));
      const callsBeforeApply = execCalls;
      await userEvent.click(await screen.findByRole("button", { name: "Apply" }));

      expect(screen.getByDisplayValue("SELECT 1 AS ok")).toBeInTheDocument();
      expect(execCalls).toBe(callsBeforeApply);
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });

    it("a failed fix stream surfaces the error in the Ask tab", async () => {
      enableAi();
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        failingQueryResults(),
        http.post(`${BASE}/api/queries/ask`, () =>
          askSse([{ type: "failed", status: "generation_failed", message: "Couldn't generate a fix." }]),
        ),
      );

      await renderIntoErrorState();
      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));

      expect(await screen.findByText(/Couldn't generate a fix\./)).toBeInTheDocument();
    });
    async function regenerateAfterFix(editQuestion: boolean, editSql?: string) {
      enableAi();
      const bodies: Record<string, unknown>[] = [];
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        failingQueryResults(),
        http.post(`${BASE}/api/queries/ask`, async ({ request }) => {
          bodies.push((await request.json()) as Record<string, unknown>);
          return askSse([{ type: "final", sql: "SELECT 1 AS ok", explanation: null, served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1, executed: true, error: null }]);
        }),
      );
      await renderIntoErrorState();
      await userEvent.click(screen.getByRole("button", { name: "✨ Fix with AI" }));
      await screen.findByRole("button", { name: "Apply" });
      if (editQuestion) {
        await userEvent.type(screen.getByLabelText(/ask ai a question about your data/i), " please");
      }
      if (editSql) {
        await userEvent.click(screen.getByRole("button", { name: "SQL Editor" }));
        fireEvent.change(screen.getByTestId("sql-editor"), { target: { value: editSql } });
        await userEvent.click(screen.getByRole("button", { name: "✨ Ask with AI" }));
      }
      await userEvent.click(screen.getByRole("button", { name: /regenerate/i }));
      await waitFor(() => expect(bodies).toHaveLength(2));
      return { bodies };
    }

    it("Regenerate with the unchanged Fix question stays a fix run", async () => {
      const { bodies } = await regenerateAfterFix(false);
      expect(bodies[1].existing_sql).toBe(bodies[0].existing_sql);
      expect(bodies[1].error).toBe(bodies[0].error);
    });

    it("Regenerate after editing the Fix question becomes a plain ask", async () => {
      const { bodies } = await regenerateAfterFix(true, "SELECT 42 AS edited");
      expect(bodies[1].error).toBeUndefined();
      expect(bodies[1].existing_sql).toBe("SELECT 42 AS edited");
      expect(bodies[0].existing_sql).not.toBe("SELECT 42 AS edited");
    });
  });

  describe("Auto Visualize", () => {
    const AV_COLUMNS = [
      { name: "status", friendly_type: "string" },
      { name: "count", friendly_type: "integer" },
    ];
    const AV_ROWS = [{ status: "ok", count: 3 }];

    function queryResultsHandler(
      columns: { name: string; friendly_type: string }[],
      rows: Record<string, unknown>[],
    ) {
      return http.post(`${BASE}/api/query_results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: { columns, rows, runtime_seconds: 0.01 },
            runtime: 0.01,
            retrieved_at: "",
          },
        }),
      );
    }

    async function renderWithResults(path: string) {
      renderEditor(path);
      const executeBtn = await screen.findByRole("button", { name: /execute/i });
      await waitFor(() => expect(executeBtn).not.toBeDisabled());
      await userEvent.click(executeBtn);
      // "Updated" is unique to the meta line; /row/i also matches ResultsTable pagination.
      await waitFor(() => expect(screen.getByText(/Updated/i)).toBeInTheDocument());
    }

    it("no trigger for an unsaved (new) query, even with inline results", async () => {
      server.use(queryResultsHandler(AV_COLUMNS, AV_ROWS));

      await renderWithResults("/queries/new");

      expect(screen.queryByRole("button", { name: /Auto Visualize/ })).not.toBeInTheDocument();
    });

    it("trigger renders for a saved query with inline results", async () => {
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        queryResultsHandler(AV_COLUMNS, AV_ROWS),
      );

      await renderWithResults("/queries/42");

      expect(screen.getByRole("button", { name: /Auto Visualize/ })).toBeInTheDocument();
    });

    it("a successful suggestion reaches VizPanel as an autoCreateRequest with the ✨-prefixed name", async () => {
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        queryResultsHandler(AV_COLUMNS, AV_ROWS),
        http.post(`${BASE}/api/queries/suggest_visualization`, () =>
          HttpResponse.json({
            status: "ok",
            type: "CHART",
            title: "Status breakdown",
            options: { globalSeriesType: "bar" },
            served_by: "gemini",
          }),
        ),
      );

      await renderWithResults("/queries/42");
      await userEvent.click(screen.getByRole("button", { name: "✨ Auto Visualize" }));

      const panel = await screen.findByTestId("viz-panel");
      await waitFor(() =>
        expect(JSON.parse(panel.dataset.auto!)).toEqual({
          type: "CHART",
          name: "✨ Status breakdown",
          options: { globalSeriesType: "bar" },
        }),
      );
    });

    it("the request body is exactly column metadata + row_count, no row values", async () => {
      let body: Record<string, unknown> | null = null;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        queryResultsHandler(AV_COLUMNS, AV_ROWS),
        http.post(`${BASE}/api/queries/suggest_visualization`, async ({ request }) => {
          body = (await request.json()) as Record<string, unknown>;
          return HttpResponse.json({
            status: "ok",
            type: "TABLE",
            title: "T",
            options: {},
            served_by: "gemini",
          });
        }),
      );

      await renderWithResults("/queries/42");
      await userEvent.click(screen.getByRole("button", { name: "✨ Auto Visualize" }));

      await waitFor(() => expect(body).not.toBeNull());
      expect(body).toEqual({
        columns: [
          { name: "status", friendly_type: "string" },
          { name: "count", friendly_type: "integer" },
        ],
        row_count: 1,
      });
    });

    it("shows 'Visualizing…' and disables the trigger while the request is pending", async () => {
      let resolveSuggest!: (r: Response) => void;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        queryResultsHandler(AV_COLUMNS, AV_ROWS),
        http.post(
          `${BASE}/api/queries/suggest_visualization`,
          () =>
            new Promise<Response>((resolve) => {
              resolveSuggest = resolve;
            }),
        ),
      );

      await renderWithResults("/queries/42");
      await userEvent.click(screen.getByRole("button", { name: "✨ Auto Visualize" }));

      const pendingBtn = await screen.findByRole("button", { name: /Visualizing/ });
      expect(pendingBtn).toBeDisabled();

      resolveSuggest(
        HttpResponse.json({
          status: "ok",
          type: "TABLE",
          title: "T",
          options: {},
          served_by: "gemini",
        }),
      );
      await waitFor(() =>
        expect(screen.getByRole("button", { name: "✨ Auto Visualize" })).toBeInTheDocument(),
      );
    });

    it("a not_configured response shows the error box with Try again, which clears the error and re-fires", async () => {
      let calls = 0;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        queryResultsHandler(AV_COLUMNS, AV_ROWS),
        http.post(`${BASE}/api/queries/suggest_visualization`, () => {
          calls += 1;
          return HttpResponse.json({
            status: "not_configured",
            message: "Auto Visualize isn't configured.",
          });
        }),
      );

      await renderWithResults("/queries/42");
      await userEvent.click(screen.getByRole("button", { name: "✨ Auto Visualize" }));

      expect(await screen.findByText("Auto Visualize isn't configured.")).toBeInTheDocument();
      expect(calls).toBe(1);

      await userEvent.click(screen.getByRole("button", { name: "Try again" }));

      await waitFor(() => expect(calls).toBe(2));
    });
  });

  describe("Stop / cancel", () => {
    afterEach(() => {
      vi.mocked(usePollJob).mockReturnValue(DEFAULT_JOB_STATE as unknown as ReturnType<typeof usePollJob>);
    });

    it("hides Stop while the initial POST is in flight (no job id yet), then shows it once a job id arrives", async () => {
      let resolvePost!: (r: Response) => void;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(
          `${BASE}/api/query_results`,
          () =>
            new Promise<Response>((resolve) => {
              resolvePost = resolve;
            }),
        ),
      );

      renderEditor("/queries/42");
      const executeBtn = await screen.findByRole("button", { name: "Execute" });
      await userEvent.click(executeBtn);

      await waitFor(() => expect(executeBtn).toBeDisabled());
      expect(screen.queryByRole("button", { name: "Stop" })).not.toBeInTheDocument();

      resolvePost(HttpResponse.json({ job: { id: "job-1", status: "pending" } }));

      expect(await screen.findByRole("button", { name: "Stop" })).toBeInTheDocument();
    });

    it("clicking Stop issues DELETE /api/jobs/<id>", async () => {
      let jobDeleteCalls = 0;
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "pending" } }),
        ),
        http.delete(`${BASE}/api/jobs/job-1`, () => {
          jobDeleteCalls += 1;
          return new HttpResponse(null, { status: 204 });
        }),
      );

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: "Execute" }));
      await userEvent.click(await screen.findByRole("button", { name: "Stop" }));

      await waitFor(() => expect(jobDeleteCalls).toBe(1));
    });

    it("shows the stopped banner and re-enables Execute once the poll observes CANCELLED", async () => {
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "cancelled" } }),
        ),
      );
      vi.mocked(usePollJob).mockReturnValue({
        ...DEFAULT_JOB_STATE,
        isDone: true,
        isCancelled: true,
      } as unknown as ReturnType<typeof usePollJob>);

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: "Execute" }));

      expect(await screen.findByText("Query stopped. No results were saved.")).toBeInTheDocument();
      expect(
        screen.getByText(/source database may keep running it/),
      ).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Execute" })).not.toBeDisabled();
      expect(screen.queryByRole("button", { name: "Stop" })).not.toBeInTheDocument();
    });

    it("a cancelJob rejection does not clear activeJobId — Stop stays, and the failure surfaces", async () => {
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "pending" } }),
        ),
        http.delete(`${BASE}/api/jobs/job-1`, () =>
          HttpResponse.json({ message: "boom" }, { status: 500 }),
        ),
      );

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: "Execute" }));
      await userEvent.click(await screen.findByRole("button", { name: "Stop" }));

      expect(await screen.findByText("boom")).toBeInTheDocument();
      expect(screen.getByRole("button", { name: "Stop" })).toBeInTheDocument();
    });

    it("does not steal focus from the editor when a run completes normally while it has focus", async () => {
      server.use(
        http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
        http.post(`${BASE}/api/query_results`, () =>
          HttpResponse.json({ job: { id: "job-1", status: "pending" } }),
        ),
      );

      renderEditor("/queries/42");
      await userEvent.click(await screen.findByRole("button", { name: "Execute" }));
      await screen.findByRole("button", { name: "Stop" });

      const editor = screen.getByTestId("sql-editor");
      editor.focus();
      expect(document.activeElement).toBe(editor);

      vi.mocked(usePollJob).mockReturnValue({
        ...DEFAULT_JOB_STATE,
        isDone: true,
        isSuccess: true,
      } as unknown as ReturnType<typeof usePollJob>);
      await userEvent.type(editor, "x");

      await waitFor(() =>
        expect(screen.queryByRole("button", { name: "Stop" })).not.toBeInTheDocument(),
      );
      expect(document.activeElement).toBe(editor);
    });
  });

});

describe("QueryEditorPage AI generated SQL", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
    );
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("renders generated SQL in a wrapped, syntax-highlighted view instead of a one-line <pre>", async () => {
    enableAi();
    server.use(
      http.post(`${BASE}/api/queries/ask`, () => askSse([{ type: "final", sql: "SELECT count(*) FROM t WHERE created_at > now()", explanation: null, served_by: "Gemini", attempts: 1, tokens_in: 1, tokens_out: 1 }])),
    );

    renderEditor("/queries/new");
    await userEvent.click(await screen.findByRole("button", { name: /ask with ai/i }));
    await userEvent.type(screen.getByLabelText(/ask ai a question about your data/i), "count rows");
    await userEvent.click(screen.getByRole("button", { name: "Generate SQL" }));

    await screen.findByRole("button", { name: "Apply" });
    // CodeMirror splits content across `.cm-line` nodes, so assert on the container text.
    const preview = document.querySelector(".cm-editor");
    expect(preview).not.toBeNull();
    expect(preview?.textContent).toContain("SELECT count(*) FROM t WHERE created_at > now()");
    expect(document.querySelector("pre")).toBeNull();
  });
});

describe("QueryEditorPage autosave (no Save button)", () => {
  function setOnline(value: boolean) {
    Object.defineProperty(window.navigator, "onLine", { value, configurable: true });
  }

  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
    );
  });

  afterEach(() => {
    vi.useRealTimers();
    setOnline(true);
    vi.mocked(VersionHistoryPanel).mockImplementation(() => <div data-testid="version-history" />);
  });

  it("debounce-saves once after typing settles, and shows Saved", async () => {
    const bodies: Record<string, unknown>[] = [];
    server.use(
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        bodies.push(body);
        return HttpResponse.json({ ...QUERY, query: body.query, working_version: 2 });
      }),
    );

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    fireEvent.change(editor, { target: { value: "SELECT 2, 3" } });
    act(() => {
      vi.advanceTimersByTime(500);
    });
    expect(bodies).toHaveLength(0);
    act(() => {
      vi.advanceTimersByTime(600);
    });
    vi.useRealTimers();

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0].query).toBe("SELECT 2, 3");
    expect(bodies[0].expected_version).toBe(1);
    expect(await screen.findByText("Saved")).toBeInTheDocument();
  });

  it("shows 'Save failed' with a retry affordance on a rejected save, and keeps the buffer", async () => {
    server.use(
      http.post(`${BASE}/api/queries/42`, () => HttpResponse.json({ message: "boom" }, { status: 500 })),
    );

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    expect(await screen.findByText("Save failed")).toBeInTheDocument();
    const retryBtn = screen.getByRole("button", { name: "Retry" });
    expect(retryBtn).toBeInTheDocument();
    expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 2");

    let succeeded = false;
    server.use(
      http.post(`${BASE}/api/queries/42`, () => {
        succeeded = true;
        return HttpResponse.json({ ...QUERY, query: "SELECT 2", working_version: 2 });
      }),
    );
    await userEvent.click(retryBtn);
    await waitFor(() => expect(succeeded).toBe(true));
    expect(await screen.findByText("Saved")).toBeInTheDocument();
  });

  it("recovers from an evicted query cache entry by refetching the working_version instead of dropping the write", async () => {
    const bodies: Record<string, unknown>[] = [];
    let getCalls = 0;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => {
        getCalls += 1;
        return HttpResponse.json(QUERY);
      }),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        bodies.push(body);
        return HttpResponse.json({ ...QUERY, query: body.query as string, working_version: 2 });
      }),
    );

    const { queryClient } = renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");
    await waitFor(() => expect(getCalls).toBe(1));

    // Simulates the cache entry vanishing between mount fetch and save without disturbing the live
    // `useQuery` subscription.
    vi.spyOn(queryClient, "getQueryData").mockReturnValueOnce(undefined);

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    await waitFor(() => expect(getCalls).toBe(2));
    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0].expected_version).toBe(1);
    expect(await screen.findByText("Saved")).toBeInTheDocument();
  });

  it("honestly fails (never reports Saved) when the row is genuinely gone after a cache eviction", async () => {
    let getCalls = 0;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => {
        getCalls += 1;
        if (getCalls === 1) return HttpResponse.json(QUERY);
        return HttpResponse.json({ message: "not found" }, { status: 404 });
      }),
    );
    let postCalled = false;
    server.use(http.post(`${BASE}/api/queries/42`, () => { postCalled = true; return HttpResponse.json(QUERY); }));

    const { queryClient } = renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");
    await waitFor(() => expect(getCalls).toBe(1));

    vi.spyOn(queryClient, "getQueryData").mockReturnValueOnce(undefined);

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    expect(await screen.findByText("Save failed")).toBeInTheDocument();
    expect(screen.queryByText("Saved")).not.toBeInTheDocument();
    expect(postCalled).toBe(false);
  });

  it("shows 'Offline — not saved' while offline and retries automatically on reconnect", async () => {
    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    setOnline(false);
    let saveCalls = 0;
    server.use(
      http.post(`${BASE}/api/queries/42`, () => {
        saveCalls++;
        return HttpResponse.json({ ...QUERY, query: "SELECT 2", working_version: 2 });
      }),
    );

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    expect(await screen.findByText("Offline — not saved")).toBeInTheDocument();
    expect(saveCalls).toBe(0);

    setOnline(true);
    act(() => {
      window.dispatchEvent(new Event("online"));
    });
    await waitFor(() => expect(saveCalls).toBe(1));
    expect(await screen.findByText("Saved")).toBeInTheDocument();
  });

  it("renders no Save button anywhere, wide or in the overflow menu", async () => {
    renderEditor("/queries/42");
    await screen.findByRole("button", { name: "Execute" });
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();

    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "More actions" }));
    await screen.findByRole("menu");
    expect(screen.queryByRole("menuitem", { name: "Save" })).not.toBeInTheDocument();
  });

  function conflictResponse() {
    return HttpResponse.json(
      { message: "stale", error_code: "conflict", working_version: 9 },
      { status: 409 },
    );
  }

  it("shows the reload banner (not 'Save failed') when autosave hits a 409", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    expect(
      await screen.findByText("This query was changed by someone else. Reload to get their version."),
    ).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Reload" })).toBeInTheDocument();
    expect(screen.queryByText("Save failed")).not.toBeInTheDocument();
    expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 2");
  });

  it("shows the reload banner when publish hits a 409", async () => {
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.post(`${BASE}/api/queries/42/publish`, () => conflictResponse()),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Publish" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Publish" }));

    expect(
      await screen.findByText("This query was changed by someone else. Reload to get their version."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("shows the reload banner when discard hits a 409", async () => {
    const publishedPending = { ...QUERY, published: true, has_unpublished_changes: true };
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(publishedPending)),
      http.post(`${BASE}/api/queries/42/discard`, () => conflictResponse()),
    );

    renderEditor("/queries/42");
    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Discard changes" }));

    expect(
      await screen.findByText("This query was changed by someone else. Reload to get their version."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("clicking Reload refetches the query, rehydrates the editor, and clears the banner", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    await screen.findByRole("button", { name: "Reload" });

    // Different name AND SQL, so a reload must adopt both.
    const theirVersion = { ...QUERY, name: "Their name", query: "SELECT 'theirs'", working_version: 9 };
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(theirVersion)));

    await userEvent.click(screen.getByRole("button", { name: "Reload" }));

    await waitFor(() =>
      expect(
        screen.queryByText("This query was changed by someone else. Reload to get their version."),
      ).not.toBeInTheDocument(),
    );
    expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 'theirs'");
    expect(screen.getByDisplayValue("Their name")).toBeInTheDocument();
  });

  it("Reload does not spuriously re-save the just-loaded server copy (autosave baseline resets with it)", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByRole("button", { name: "Reload" });

    const theirVersion = { ...QUERY, name: "Their name", query: "SELECT 'theirs'", working_version: 9 };
    let postCount = 0;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(theirVersion)),
      http.post(`${BASE}/api/queries/42`, () => {
        postCount++;
        return HttpResponse.json({ ...theirVersion, working_version: 10 });
      }),
    );

    await userEvent.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 'theirs'"));

    // Without markClean() autosave would re-save already-current content.
    vi.useFakeTimers();
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    vi.useRealTimers();
    await new Promise((r) => setTimeout(r, 20));
    expect(postCount).toBe(0);
  });

  it("a successful discard replaces the draft buffer with the published SQL, not the discarded draft", async () => {
    const draftPending = { ...QUERY, published: true, has_unpublished_changes: true, query: "SELECT 'draft'" };
    const discarded = {
      ...QUERY,
      published: true,
      has_unpublished_changes: false,
      query: "SELECT 'published' AS marker",
      working_version: 2,
    };
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(draftPending)),
      http.post(`${BASE}/api/queries/42/discard`, () => HttpResponse.json(discarded)),
    );

    renderEditor("/queries/42");
    expect(await screen.findByTestId("sql-editor")).toHaveValue("SELECT 'draft'");
    expect(await screen.findByText("Unpublished changes")).toBeInTheDocument();

    await userEvent.click(await screen.findByRole("button", { name: "Discard changes" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Discard changes" }));

    await waitFor(() =>
      expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 'published' AS marker"),
    );
    expect(screen.queryByText("Unpublished changes")).not.toBeInTheDocument();

    let postCount = 0;
    server.use(
      http.post(`${BASE}/api/queries/42`, () => {
        postCount++;
        return HttpResponse.json({ ...discarded, working_version: 3 });
      }),
    );
    vi.useFakeTimers();
    act(() => {
      vi.advanceTimersByTime(2000);
    });
    vi.useRealTimers();
    // Real wait (the timer predates fake timers) so a wrongly armed debounce would fire.
    await new Promise((r) => setTimeout(r, 1200));
    expect(postCount).toBe(0);

    // Editor must stay live after discard; reset the counter to isolate this save.
    postCount = 0;
    const editor = screen.getByTestId("sql-editor");
    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 'published' AS marker, 1" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await waitFor(() => expect(postCount).toBe(1));
  });

  it("conflict -> Reload (server row identical to the buffer) -> next edit still saves", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByRole("button", { name: "Reload" });

    const sameVersion = { ...QUERY, query: "SELECT 2", working_version: 9 };
    let postCount = 0;
    let lastBody: Record<string, unknown> | null = null;
    server.use(
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(sameVersion)),
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        postCount++;
        lastBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ ...sameVersion, query: lastBody.query as string, working_version: 10 });
      }),
    );

    await userEvent.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 2"));

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 3" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    await waitFor(() => expect(postCount).toBe(1));
    expect((lastBody as unknown as Record<string, unknown>).query).toBe("SELECT 3");
  });

  it("after that same Reload, beforeunload is still armed for the pending edit", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByRole("button", { name: "Reload" });

    const sameVersion = { ...QUERY, query: "SELECT 2", working_version: 9 };
    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(sameVersion)));

    await userEvent.click(screen.getByRole("button", { name: "Reload" }));
    await waitFor(() => expect(screen.queryByRole("button", { name: "Reload" })).not.toBeInTheDocument());

    fireEvent.change(editor, { target: { value: "SELECT 3" } });
    // Flush the debounce effect's pendingRef before the synchronous beforeunload.
    await new Promise((r) => setTimeout(r, 20));

    const event = new Event("beforeunload", { cancelable: true });
    window.dispatchEvent(event);

    expect(event.defaultPrevented).toBe(true);
  });

  it("a failed Reload keeps the conflict banner and does not rehydrate", async () => {
    server.use(http.post(`${BASE}/api/queries/42`, () => conflictResponse()));

    renderEditor("/queries/42");
    const editor = await screen.findByTestId("sql-editor");

    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 2" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();
    await screen.findByRole("button", { name: "Reload" });

    server.use(http.get(`${BASE}/api/queries/42`, () => new HttpResponse(null, { status: 500 })));

    await userEvent.click(screen.getByRole("button", { name: "Reload" }));

    expect(
      await screen.findByText("Couldn't reload this query. Check your connection and try again."),
    ).toBeInTheDocument();
    expect(
      screen.getByText("This query was changed by someone else. Reload to get their version."),
    ).toBeInTheDocument();
    expect(screen.getByTestId("sql-editor")).toHaveValue("SELECT 2");
  });

  it("version-history Restore of the current version -> next edit still saves (drives handleReload via VersionHistoryPanel's onReverted, the same code path the conflict banner's Reload button uses)", async () => {
    // Persist the override for the whole test: a Once would be consumed by an earlier render.
    vi.mocked(VersionHistoryPanel).mockImplementation((props) => (
      <button onClick={() => props.onReverted("Reverted to version 1")}>Trigger Restore</button>
    ));

    renderEditor("/queries/42");
    await screen.findByTestId("sql-editor");

    server.use(http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)));
    let postCount = 0;
    let lastBody: Record<string, unknown> | null = null;
    server.use(
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        postCount++;
        lastBody = (await request.json()) as Record<string, unknown>;
        return HttpResponse.json({ ...QUERY, query: lastBody.query as string, working_version: 2 });
      }),
    );

    await userEvent.click(await screen.findByRole("button", { name: "Trigger Restore" }));
    await screen.findByText("Reverted to version 1");

    const editor = screen.getByTestId("sql-editor");
    vi.useFakeTimers();
    fireEvent.change(editor, { target: { value: "SELECT 99" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    await waitFor(() => expect(postCount).toBe(1));
    expect((lastBody as unknown as Record<string, unknown>).query).toBe("SELECT 99");
  });
});

describe("QueryEditorPage Monitors card", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json(QUERY)),
      http.get(`${BASE}/api/queries/42/acl`, () => HttpResponse.json([])),
    );
  });

  it("shows the empty-state copy when no monitors watch this query", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])));

    const { container } = renderEditor("/queries/42");

    expect(await screen.findByText("No monitors watch this query")).toBeInTheDocument();
    expect(
      screen.getByText("A monitor checks this query on a schedule and alerts you when the data looks wrong."),
    ).toBeInTheDocument();
    expect(container.querySelector("svg.lucide-bell-ring")).toBeInTheDocument();
  });

  it("lists monitors with their firing state and enabled/paused/muted schedule state", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () =>
        HttpResponse.json([
          { id: 1, name: "Alpha", options: { muted: false }, state: "ok" },
          { id: 2, name: "Beta", options: { muted: true }, state: "triggered" },
          { id: 3, name: "Gamma", options: { muted: false }, state: "ok" },
        ]),
      ),
      http.get(`${BASE}/api/schedules`, () =>
        HttpResponse.json([
          {
            id: 10,
            entity_type: "monitor",
            entity_id: 1,
            spec: { kind: "interval", every_secs: 300 },
            timezone: "UTC",
            enabled: true,
            paused_until: null,
            auto_paused_at: null,
            overlap_policy: "skip",
            next_run_at: null,
            consecutive_failures: 0,
            last_run: null,
          },
          {
            id: 11,
            entity_type: "monitor",
            entity_id: 3,
            spec: { kind: "interval", every_secs: 300 },
            timezone: "UTC",
            enabled: false,
            paused_until: null,
            auto_paused_at: null,
            overlap_policy: "skip",
            next_run_at: null,
            consecutive_failures: 0,
            last_run: null,
          },
        ]),
      ),
    );

    renderEditor("/queries/42");

    // Alpha: has an enabled schedule, not muted -> Active + OK
    expect(await screen.findByRole("link", { name: "Alpha" })).toHaveAttribute("href", "/monitors/1");
    // Beta: muted overrides any schedule state -> Muted + Alerting (no schedule row for it at all)
    expect(await screen.findByRole("link", { name: "Beta" })).toHaveAttribute("href", "/monitors/2");
    // Gamma: schedule row exists but disabled -> Paused + OK
    expect(await screen.findByRole("link", { name: "Gamma" })).toHaveAttribute("href", "/monitors/3");

    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.getByText("Muted")).toBeInTheDocument();
    expect(screen.getByText("Paused")).toBeInTheDocument();
    expect(screen.getByText("Alerting")).toBeInTheDocument();
    expect(screen.getAllByText("OK")).toHaveLength(2);
  });

  it("Add monitor navigates to /monitors/new with this query's id prefilled", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])));

    renderEditor("/queries/42");

    await userEvent.click(await screen.findByRole("button", { name: "Add monitor" }));

    expect(navigateSpy).toHaveBeenCalledWith("/monitors/new?query_id=42");
  });

  it("disables (but still shows) Add monitor without the create_monitor capability", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/session`, () =>
        HttpResponse.json({
          user: {
            id: 1,
            name: "Test User",
            email: "test@example.com",
            groups: [1],
            org_id: 1,
            is_invitation_pending: false,
            disabled_at: null,
            profile_image_url: null,
            details: {},
            created_at: "2025-01-01T00:00:00Z",
            updated_at: "2025-01-01T00:00:00Z",
          },
          org: { id: 1, name: "Test Org", slug: "test-org", settings: {} },
          permissions: ["view_query", "execute_query", "view_monitor"],
          is_admin: false,
        }),
      ),
    );

    renderEditor("/queries/42");

    const addBtn = await screen.findByRole("button", { name: "Add monitor" });
    expect(addBtn).toBeDisabled();
  });
});

describe("QueryEditorPage tags + description", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json({ ...QUERY, tags: ["existing"] })),
    );
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("includes the description in the autosave POST body", async () => {
    const bodies: Record<string, unknown>[] = [];
    server.use(
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        bodies.push(body);
        return HttpResponse.json({
          ...QUERY,
          tags: ["existing"],
          description: body.description,
          working_version: 2,
        });
      }),
    );

    renderEditor("/queries/42");
    const descriptionInput = await screen.findByPlaceholderText("Add a description…");

    vi.useFakeTimers();
    fireEvent.change(descriptionInput, { target: { value: "A helpful description" } });
    act(() => {
      vi.advanceTimersByTime(1100);
    });
    vi.useRealTimers();

    await waitFor(() => expect(bodies).toHaveLength(1));
    expect(bodies[0].description).toBe("A helpful description");
  });

  it("adding a tag via Enter merges it in, removing a chip filters it out — each fires POST /api/queries/:id immediately", async () => {
    let lastTagsBody: string[] | undefined;
    server.use(
      http.post(`${BASE}/api/queries/42`, async ({ request }) => {
        const body = (await request.json()) as { tags?: string[] };
        lastTagsBody = body.tags;
        return HttpResponse.json({ ...QUERY, tags: body.tags ?? ["existing"], working_version: 2 });
      }),
    );

    renderEditor("/queries/42");
    const tagInput = await screen.findByPlaceholderText("add tag…");
    fireEvent.change(tagInput, { target: { value: "new-tag" } });
    fireEvent.keyDown(tagInput, { key: "Enter" });

    await waitFor(() => expect(lastTagsBody).toEqual(["existing", "new-tag"]));
    expect(await screen.findByText("new-tag")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Remove tag existing" }));

    await waitFor(() => expect(lastTagsBody).toEqual(["new-tag"]));
  });

  it("offers the datalist's org tags minus tags already applied to this query", async () => {
    server.use(
      http.get(`${BASE}/api/queries/tags`, () =>
        HttpResponse.json({ tags: ["existing", "revenue", "ops"] }),
      ),
    );

    renderEditor("/queries/42");
    await screen.findByPlaceholderText("add tag…");

    await waitFor(() => {
      const values = Array.from(document.querySelectorAll("#query-tag-suggestions option")).map(
        (o) => o.getAttribute("value"),
      );
      expect(values).toEqual(["revenue", "ops"]);
    });
  });
});

describe("QueryEditorPage result footnote slot", () => {
  const RETRIEVED_AT = new Date(Date.now() - 9 * 60_000).toISOString();

  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources`, () => HttpResponse.json({ data_sources: [DS], can_create: true })),
      http.get(`${BASE}/api/data_sources/1/schema`, () => HttpResponse.json({ schema: [] })),
      http.get(`${BASE}/api/query_snippets`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/queries/42`, () => HttpResponse.json({ ...QUERY, latest_query_data_id: 99 })),
      http.get(`${BASE}/api/queries/42/results`, () =>
        HttpResponse.json({
          query_result: {
            id: 99,
            data_source_id: 1,
            query_hash: "h",
            query: "SELECT 1",
            data: {
              columns: [{ name: "count", type: "integer", friendly_name: "count" }],
              rows: [{ count: 1 }],
              runtime_seconds: 0.01,
            },
            runtime: 0.01,
            retrieved_at: RETRIEVED_AT,
          },
        }),
      ),
    );
  });

  afterEach(() => {
    extensions.length = 0;
  });

  it("renders_result_footnote_slot_when_provided", async () => {
    extensions.push({
      navItems: [],
      routes: [],
      resultFootnoteSlot: (p) => (
        <span data-testid="ext-footnote">
          {p.placement}|{p.retrievedAt}|{String(p.publicView)}
        </span>
      ),
    });

    renderEditor("/queries/42");

    const note = await screen.findByTestId("ext-footnote");
    expect(note).toHaveTextContent(`editor|${RETRIEVED_AT}|undefined`);
    expect(note.closest("p")).toHaveTextContent(/Updated 9 min ago/);
  });

  it("renders_nothing_without_extension", async () => {
    renderEditor("/queries/42");

    const meta = (await screen.findByText(/Updated 9 min ago/i)).closest("p");
    expect(meta).not.toBeNull();
    expect(meta).not.toHaveTextContent("·  ·");
    expect(meta?.textContent?.trim().endsWith("Updated 9 min ago")).toBe(true);
  });
});
