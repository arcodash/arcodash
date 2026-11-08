import { describe, it, expect, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MonitorPage } from "../MonitorPage";
import type * as ApiModule from "@/lib/api";

vi.mock("@/lib/permissions", () => ({
  usePermissions: () => ({ has: () => true, isLoading: false, hasAny: () => true, isAdmin: true, keys: new Set() }),
}));
vi.mock("@/components/scheduling/ScheduleSection", () => ({
  ScheduleSection: () => null,
  MONITOR_FLAVOR: {},
}));
vi.mock("@/lib/api", async (importOriginal) => {
  const actual = await importOriginal<typeof ApiModule>();
  const api = Object.fromEntries(
    Object.keys(actual.api).map((k) => [k, vi.fn(async () => [])]),
  ) as unknown as typeof actual.api;
  return { ...actual, api };
});

import { api } from "@/lib/api";

const mocked = api as unknown as Record<string, ReturnType<typeof vi.fn>>;

function monitor(overrides: Record<string, unknown> = {}) {
  return {
    id: 5,
    name: "Backlog",
    query: { id: 2, name: "Backlog query" },
    options: { column: "backlog", op: "<", value: 100 },
    state: "ok",
    kind: "threshold",
    user: { id: 1, name: "Ann" },
    org_id: 1,
    visibility: "private",
    created_at: "2026-01-01T00:00:00Z",
    updated_at: "2026-01-01T00:00:00Z",
    acked_at: null,
    acked_by_email: null,
    escalation_step: 0,
    unevaluable_reason: null,
    escalation: [],
    caller: { object_role: "owner", can_edit: true, can_delete: true },
    ...overrides,
  };
}

function setup(m: ReturnType<typeof monitor>) {
  mocked.getMonitor.mockResolvedValue(m);
  mocked.updateMonitor.mockResolvedValue(m);
  mocked.getMonitorObservations.mockResolvedValue([]);
  mocked.getLatestQueryResult.mockResolvedValue({
    query_result: {
      data: { rows: [], columns: [{ name: "backlog", friendly_type: "integer" }] },
    },
  });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/monitors/5"]}>
        <Routes>
          <Route path="/monitors/:id" element={<MonitorPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("MonitorPage alert type", () => {
  it("nulls op/value when switching to baseline and sensitivity when switching back", async () => {
    setup(monitor());
    expect(await screen.findByDisplayValue("100")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Baseline" }));
    expect(screen.queryByLabelText(/Value/)).not.toBeInTheDocument();
    expect(screen.getByText(/barely changes, alerts at about a 52% move/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(mocked.updateMonitor).toHaveBeenCalledTimes(1));
    const options = mocked.updateMonitor.mock.calls[0][1].options;
    expect(options).toMatchObject({ column: "backlog", kind: "baseline", sensitivity: "medium" });
    expect(options).toMatchObject({ op: null, value: null });

    await userEvent.click(screen.getByRole("button", { name: "Threshold" }));
    expect(screen.getByDisplayValue("100")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(mocked.updateMonitor).toHaveBeenCalledTimes(2));
    expect(mocked.updateMonitor.mock.calls[1][1].options).toMatchObject({ op: "<", value: 100, sensitivity: null });
  });

  it("shows the learning badge for a baseline monitor", async () => {
    setup(monitor({ kind: "baseline", learning: { have: 3, need: 7 }, options: { column: "backlog", kind: "baseline", sensitivity: "high" } }));

    expect(await screen.findByTestId("monitor-learning-badge")).toHaveTextContent(/^Learning/);
    expect(screen.getByText(/about a 37% move/)).toBeInTheDocument();
  });

  it("opens the learning popover with progress", async () => {
    setup(monitor({ kind: "baseline", learning: { have: 3, need: 7 }, options: { column: "backlog", kind: "baseline", sensitivity: "high" } }));
    mocked.getSchedule.mockResolvedValue({
      id: 1,
      entity_type: "monitor",
      entity_id: 5,
      spec: { kind: "interval", every_secs: 3600 },
      timezone: "UTC",
      enabled: true,
      paused_until: null,
      auto_paused_at: null,
      overlap_policy: "skip",
      next_run_at: new Date(Date.now() + 600000).toISOString(),
      consecutive_failures: 0,
      last_run: null,
    });

    await userEvent.click(await screen.findByTestId("monitor-learning-badge"));
    expect(await screen.findByText("Learning the usual range")).toBeInTheDocument();
    expect(screen.getAllByText("3 of 7 checks").length).toBeGreaterThan(0);
  });
});

describe("MonitorPage AI prefill", () => {
  const suggestion = {
    name: "Signups drop",
    kind: "threshold" as const,
    column: "signups",
    sensitivity: null,
    op: "<",
    value: 40,
    definition: "d",
    why: "Signups are the funnel top.",
    confidence: "medium" as const,
  };

  function setupNew(state: unknown, search = "") {
    mocked.listQueries.mockResolvedValue([{ id: 7, name: "Q7", latest_query_data_id: 1 }]);
    mocked.listDataSources.mockResolvedValue({ data_sources: [{ id: 3, name: "Warehouse" }], can_create: true });
    mocked.getLatestQueryResult.mockResolvedValue({
      query_result: { data: { rows: [], columns: [{ name: "signups", friendly_type: "integer" }] } },
    });
    mocked.createMonitor.mockResolvedValue({ id: 99 });
    mocked.createMonitorSubscription.mockResolvedValue({});
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={[{ pathname: "/monitors/new", search, state }]}>
          <Routes>
            <Route path="/monitors/new" element={<MonitorPage />} />
            <Route path="/monitors/:id" element={<div>detail</div>} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );
  }

  it("prefills the normal form from aiSuggestion", async () => {
    setupNew({ aiSuggestion: suggestion }, "?query_id=7");
    expect(await screen.findByDisplayValue("Signups drop")).toBeInTheDocument();
    expect(screen.getByDisplayValue("40")).toBeInTheDocument();
    expect(screen.getByTestId("monitor-ai-banner")).toHaveTextContent("AI suggestion");
    expect(screen.getByTestId("monitor-ai-banner")).toHaveTextContent("Signups are the funnel top.");
    expect(screen.getByTestId("monitor-guessed-hint")).toBeInTheDocument();
    expect(screen.queryByTestId("monitor-draft-sql")).not.toBeInTheDocument();
    expect(await screen.findByTestId("monitor-query-select")).toBeInTheDocument();
  });

  it("draft mode shows SQL, hides the picker and saves via the create chain", async () => {
    mocked.createQuery.mockResolvedValue({ id: 50, working_version: 2 });
    mocked.publishQuery.mockResolvedValue({});
    setupNew({
      aiDraft: { sql: "SELECT 1 AS signups", data_source_id: 3, query_name: "My q", suggestion, columns: [{ name: "signups", friendly_type: "integer" }] },
    });
    expect(await screen.findByTestId("monitor-draft-sql")).toHaveTextContent("SELECT 1 AS signups");
    expect(screen.queryByTestId("monitor-query-select")).not.toBeInTheDocument();
    expect(screen.getByTestId("monitor-ai-banner")).toHaveTextContent("AI draft");
    expect(await screen.findByText("Runs on Warehouse.")).toBeInTheDocument();

    const qn = screen.getByTestId("monitor-draft-query-name");
    await userEvent.clear(qn);
    await userEvent.type(qn, "Edited");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(mocked.createQuery).toHaveBeenCalledWith(expect.objectContaining({ name: "Edited", data_source_id: 3, query: "SELECT 1 AS signups" })));
    expect(mocked.publishQuery).toHaveBeenCalledWith(50, 2);
    expect(await screen.findByText("detail")).toBeInTheDocument();
  });

  it("draft mode honors the notify toggle", async () => {
    mocked.createQuery.mockResolvedValue({ id: 52, working_version: 1 });
    mocked.publishQuery.mockResolvedValue({});
    const sub = mocked.createMonitorSubscription;
    sub.mockClear();
    sub.mockResolvedValue({});
    setupNew({ aiDraft: { sql: "SELECT 1", data_source_id: 3, query_name: "Q", suggestion } });
    await userEvent.click(await screen.findByRole("switch"));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("detail")).toBeInTheDocument();
    expect(sub).not.toHaveBeenCalled();
  });

  it("blank query name disables Save; a retry resumes without recreating the query", async () => {
    mocked.createQuery.mockClear();
    mocked.createQuery.mockResolvedValue({ id: 51, working_version: 1 });
    mocked.publishQuery.mockResolvedValue({});
    setupNew({ aiDraft: { sql: "SELECT 1", data_source_id: 3, query_name: "", suggestion } });
    const save = await screen.findByRole("button", { name: "Save" });
    expect(save).toBeDisabled();
    await userEvent.type(screen.getByTestId("monitor-draft-query-name"), "Q");
    expect(save).toBeEnabled();

    mocked.createMonitor.mockRejectedValueOnce(new Error("boom"));
    await userEvent.click(save);
    expect(await screen.findByText(/The query "Q" was created/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText("detail")).toBeInTheDocument();
    expect(mocked.createQuery).toHaveBeenCalledTimes(1);
  });
});
