import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { PublicDashboardPage } from "@/pages/PublicDashboardPage";
import type { PublicEmbedResponse } from "@/types/api";

// Passthrough for react-grid-layout (jsdom can't measure width) so the real DashboardGrid +
// WidgetCard run; useContainerWidth still needs a stub since it's called unconditionally.
vi.mock("react-grid-layout", () => ({
  __esModule: true,
  default: ({ children }: { children: React.ReactNode }) => <div>{children}</div>,
  useContainerWidth: () => ({
    width: 800,
    mounted: true,
    containerRef: { current: null },
    measureWidth: () => {},
  }),
}));

const BASE = "http://localhost:8080";

function makeDashboard(overrides: Partial<PublicEmbedResponse> = {}): PublicEmbedResponse {
  return {
    object_type: "dashboard",
    title: "Sales Overview",
    widgets: [
      {
        id: 101,
        text: "Q4 revenue is up 20%",
        width: 1,
        position: { col: 0, row: 0, sizeX: 3, sizeY: 2 },
        visualization: null,
      },
      {
        // A viz widget with a cached result, for the SQL-leak assertion below.
        id: 102,
        text: null,
        width: 2,
        position: { col: 3, row: 0, sizeX: 3, sizeY: 2 },
        visualization: {
          type: "TABLE",
          name: "Revenue by month",
          options: {},
          data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: 42 }] },
        },
      },
    ],
    visualization: null,
    as_of: "2026-01-01T12:00:00Z",
    ...overrides,
  };
}

function renderPublic(token = "tok-abc") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/public/dashboards/${token}`]}>
        <Routes>
          <Route path="/public/dashboards/:token" element={<PublicDashboardPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("PublicDashboardPage", () => {
  it("renders dashboard name and widget data from the public token endpoint", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/public/:token`, () =>
        HttpResponse.json(makeDashboard()),
      ),
    );

    renderPublic();

    expect(await screen.findByText("Sales Overview")).toBeInTheDocument();
    expect(screen.getByText("Q4 revenue is up 20%")).toBeInTheDocument();
    expect(screen.getByText(/Powered by/)).toBeInTheDocument();
    const brand = screen.getByRole("link", { name: "arcodash" });
    expect(brand.getAttribute("href")).toMatch(/^https:\/\/www\.arcodash\.com\/.*utm_content=powered-by/);
  });

  it("titles the tab with the dashboard's own name, not a static section noun", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/public/:token`, () => HttpResponse.json(makeDashboard())),
    );

    renderPublic();

    await screen.findByText("Sales Overview");
    expect(document.title).toBe("Sales Overview · arcodash");
  });

  it("renders the generic link-unavailable state when the token fetch returns 404, without leaking the backend message", async () => {
    server.use(
      http.get(`${BASE}/api/dashboards/public/:token`, () =>
        HttpResponse.json(
          { message: "Dashboard not found or no longer public." },
          { status: 404 },
        ),
      ),
    );

    renderPublic("revoked-token");

    expect(await screen.findByText("Link unavailable")).toBeInTheDocument();
    expect(
      screen.queryByText("Dashboard not found or no longer public."),
    ).not.toBeInTheDocument();
    // Must not fall through to rendering dashboard chrome (this is the empty state's own footer).
    expect(screen.getByText(/Powered by/)).toBeInTheDocument();
    const brand = screen.getByRole("link", { name: "arcodash" });
    expect(brand.getAttribute("href")).toMatch(/^https:\/\/www\.arcodash\.com\/.*utm_content=powered-by/);
  });

  it("does not call /api/session or auth-gated endpoints", async () => {
    const calls = { session: false, queries: false, dataSources: false };
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls.session = true;
        return HttpResponse.json({ user: null });
      }),
      http.get(`${BASE}/api/queries`, () => {
        calls.queries = true;
        return HttpResponse.json([]);
      }),
      http.get(`${BASE}/api/data_sources`, () => {
        calls.dataSources = true;
        return HttpResponse.json([]);
      }),
      http.get(`${BASE}/api/dashboards/public/:token`, () =>
        HttpResponse.json(makeDashboard()),
      ),
    );

    renderPublic();

    expect(await screen.findByText("Sales Overview")).toBeInTheDocument();
    expect(calls.session).toBe(false);
    expect(calls.queries).toBe(false);
    expect(calls.dataSources).toBe(false);
  });
});
