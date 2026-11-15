import { describe, it, expect, vi, afterEach } from "vitest";
import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { useEffect } from "react";
import { server } from "./msw-server";
import { AuthProvider } from "@/lib/auth";
import type { Dashboard, Widget } from "@/types/api";

vi.mock("@/components/dashboard/DashboardGrid", () => ({
  DashboardGrid: ({
    widgets,
    onResultShown,
  }: {
    widgets: Widget[];
    onResultShown?: (id: number, at: string) => void;
  }) => {
    useEffect(() => {
      for (const w of widgets) {
        if (w.freshness?.retrieved_at) onResultShown?.(w.id, w.freshness.retrieved_at);
      }
    }, [widgets, onResultShown]);
    return <div data-testid="dashboard-grid" />;
  },
}));

vi.mock("@/components/dashboard/AddWidgetDialog", () => ({
  AddWidgetDialog: () => null,
}));

import { DashboardPage } from "@/pages/DashboardPage";

const BASE = "http://localhost:8080";
const T10 = "2026-11-15T10:00:00Z";
const T11 = "2026-11-15T10:05:00Z";

function vizWidget(retrievedAt: string): Widget {
  return {
    id: 10,
    dashboard_id: 1,
    visualization_id: 20,
    visualization: {
      id: 20,
      query_id: 5,
      type: "TABLE",
      name: "V",
      options: {},
      created_at: "",
      updated_at: "",
      query: { id: 5, name: "Q", data_source_id: 1, parameter_names: [], options: {}, verified: false, is_published: true },
    },
    freshness: { retrieved_at: retrievedAt, expected_interval_secs: null, last_run_status: "success" },
    text: null,
    width: 1,
    options: {},
    viewer_can_read_data: true,
    created_at: "",
    updated_at: "",
  } as Widget;
}

function makeDashboard(retrievedAt: string): Dashboard {
  return {
    id: 1,
    slug: "sales",
    name: "Sales Dashboard",
    user_id: 1,
    visibility: "workspace",
    layout: null,
    dashboard_filters_enabled: false,
    is_archived: false,
    published: true,
    has_unpublished_changes: false,
    tags: [],
    options: {},
    is_favorite: false,
    widgets: [vizWidget(retrievedAt)],
    created_at: "",
    updated_at: "",
    caller: { object_role: "owner", can_edit: true, can_delete: true, can_share: true },
  };
}

afterEach(() => vi.useRealTimers());

describe("DashboardPage new results", () => {
  it("shows the pill after a poll finds a newer result and hides it on Show", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/dashboards/sales`, () => {
        calls += 1;
        return HttpResponse.json(makeDashboard(calls === 1 ? T10 : T11));
      }),
    );
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/dashboards/sales"]}>
            <Routes>
              <Route path="/dashboards/:slug" element={<DashboardPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
    await screen.findByText("Sales Dashboard");
    expect(screen.queryByText(/New results available/)).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(calls).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000);
    });
    expect(await screen.findByText("New results available for 1 tile")).toBeInTheDocument();

    await userEvent.setup({ advanceTimers: vi.advanceTimersByTime }).click(
      screen.getByRole("button", { name: "Show new results" }),
    );
    expect(screen.queryByText(/New results available/)).toBeNull();
  });
});
