import { describe, it, expect, afterEach, vi } from "vitest";
import { MemoryRouter } from "react-router";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import { DashboardGrid } from "../DashboardGrid";
import type { Widget } from "@/types/api";

vi.mock("@/lib/permissions", () => ({ usePermissions: () => ({ has: () => true, hasAny: () => true }) }));
vi.mock("@/lib/useMediaQuery", () => ({ useMediaQuery: () => false }));

const BASE = "http://localhost:8080";

function widgetFor(id: number, queryId: number): Widget {
  return {
    id,
    dashboard_id: 1,
    visualization_id: 100 + id,
    visualization: {
      id: 100 + id,
      query_id: queryId,
      type: "TABLE",
      name: `Viz ${id}`,
      options: {},
      created_at: "2026-01-01T00:00:00.000Z",
      updated_at: "2026-01-01T00:00:00.000Z",
      query: { id: queryId, name: `Q${queryId}`, data_source_id: 1, parameter_names: [], options: {}, verified: false, is_published: true },
    },
    query_result_data: null,
    viewer_can_read_data: true,
    text: null,
    width: 1,
    options: {},
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
  };
}

describe("DashboardGrid tile refresh", () => {
  afterEach(() => server.resetHandlers());

  it("only the bumped tile refetches", async () => {
    const calls: Record<number, number> = { 1: 0, 2: 0 };
    for (const q of [1, 2]) {
      server.use(
        http.get(`${BASE}/api/queries/${q}/results`, () => {
          calls[q] += 1;
          return HttpResponse.json({
            query_result: {
              id: q,
              data_source_id: 1,
              query: "SELECT 1",
              query_hash: "h",
              data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: q * 11 }] },
              runtime: 0.01,
              retrieved_at: "2026-11-15T10:00:00Z",
            },
          });
        }),
      );
    }
    const widgets = [widgetFor(1, 1), widgetFor(2, 2)];
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const ui = (keys: Record<number, number>) => (
      <MemoryRouter><QueryClientProvider client={qc}>
        <DashboardGrid widgets={widgets} editMode={false} onDeleteWidget={() => {}} tileRefreshKeys={keys} />
      </QueryClientProvider></MemoryRouter>
    );
    const { rerender } = render(ui({}));
    await screen.findByText("11");
    await screen.findByText("22");
    expect(calls).toEqual({ 1: 1, 2: 1 });

    rerender(ui({ 2: 1 }));
    await waitFor(() => expect(calls[2]).toBe(2));
    await new Promise((r) => setTimeout(r, 50));
    expect(calls).toEqual({ 1: 1, 2: 2 });
  });
});
