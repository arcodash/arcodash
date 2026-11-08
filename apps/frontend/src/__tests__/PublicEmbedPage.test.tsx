import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { PublicEmbedPage } from "@/pages/PublicEmbedPage";
import type { PublicEmbedResponse } from "@/types/api";

// Same react-grid-layout stub as PublicDashboardPage: jsdom can't measure width, and
// useContainerWidth runs unconditionally.
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

function renderEmbed(token = "tok-abc") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[`/public/embed/${token}`]}>
        <Routes>
          <Route path="/public/embed/:token" element={<PublicEmbedPage />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function dashboardResponse(): PublicEmbedResponse {
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
    ],
    visualization: null,
    as_of: "2026-01-01T12:00:00Z",
  };
}

function vizResponse(): PublicEmbedResponse {
  return {
    object_type: "visualization",
    title: "Revenue by month",
    widgets: [],
    visualization: {
      type: "TABLE",
      name: "Revenue by month",
      options: {},
      data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: 42 }] },
    },
    as_of: "2026-01-01T12:00:00Z",
  };
}

describe("PublicEmbedPage", () => {
  it("renders a dashboard-type embed via DashboardGrid with the Public view / Powered-by chrome", async () => {
    server.use(
      http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(dashboardResponse())),
    );

    renderEmbed();

    expect(await screen.findByText("Sales Overview")).toBeInTheDocument();
    expect(screen.getByText("Q4 revenue is up 20%")).toBeInTheDocument();
    expect(screen.getByText(/Powered by/)).toBeInTheDocument();
    const brand = screen.getByRole("link", { name: "arcodash" });
    expect(brand.getAttribute("href")).toMatch(/^https:\/\/www\.arcodash\.com\/.*utm_content=powered-by/);
    expect(screen.getByText(/Public view/)).toBeInTheDocument();
  });

  it("titles the tab with the embed's own name, not a static section noun", async () => {
    server.use(
      http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(dashboardResponse())),
    );

    renderEmbed();

    await screen.findByText("Sales Overview");
    expect(document.title).toBe("Sales Overview · arcodash");
  });

  it("renders a visualization-type embed via the renderer registry, no grid", async () => {
    server.use(http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(vizResponse())));

    renderEmbed();

    expect(await screen.findByText("Revenue by month")).toBeInTheDocument();
    expect(await screen.findByText("42")).toBeInTheDocument();
  });

  it("a single-visualization embed uses a full-width, tall, responsive card instead of its intrinsic size", async () => {
    server.use(http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(vizResponse())));

    renderEmbed();
    await screen.findByText("42");

    const card = screen.getByText("42").closest(".max-w-5xl");
    expect(card).toBeTruthy();
    expect(card).toHaveClass("w-full");
  });

  it("header timestamp uses the same relative-time convention as widget freshness, not a raw locale date", async () => {
    server.use(
      http.get(`${BASE}/api/embed/:token`, () =>
        HttpResponse.json({ ...dashboardResponse(), as_of: new Date(Date.now() - 7 * 60_000).toISOString() }),
      ),
    );

    renderEmbed();

    const meta = await screen.findByText(/Public view/);
    expect(meta.textContent).toMatch(/updated (just now|\d+ min ago)/);
    expect(meta.textContent).not.toMatch(/as of/);
    expect(meta.getAttribute("title")).toBeTruthy();
  });

  it("adds a noindex robots meta tag on mount and removes it (not overwrites it) on unmount", async () => {
    server.use(
      http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(dashboardResponse())),
    );
    expect(document.querySelector('meta[name="robots"]')).toBeNull();

    const { unmount } = renderEmbed();
    await screen.findByText("Sales Overview");

    const meta = document.querySelector('meta[name="robots"]');
    expect(meta).not.toBeNull();
    expect(meta).toHaveAttribute("content", "noindex, nofollow");

    unmount();
    // Removed outright, not rewritten to "index, follow": a later client-side nav must not inherit
    // any tag.
    expect(document.querySelector('meta[name="robots"]')).toBeNull();
  });

  it("renders a generic unavailable-link state when the token fetch 404s, never the backend's raw message", async () => {
    server.use(
      http.get(`${BASE}/api/embed/:token`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
    );

    renderEmbed("revoked-token");

    expect(await screen.findByText("Link unavailable")).toBeInTheDocument();
    expect(
      screen.getByText(
        "This shared view doesn't exist or is no longer available. Ask the person who shared it for a new link.",
      ),
    ).toBeInTheDocument();
    expect(screen.getByText(/Powered by/)).toBeInTheDocument();
    const brand = screen.getByRole("link", { name: "arcodash" });
    expect(brand.getAttribute("href")).toMatch(/^https:\/\/www\.arcodash\.com\/.*utm_content=powered-by/);
    expect(screen.queryByText("not found")).not.toBeInTheDocument();
  });

  it("does not call /api/session or auth-gated endpoints", async () => {
    const calls = { session: false, queries: false };
    server.use(
      http.get(`${BASE}/api/session`, () => {
        calls.session = true;
        return HttpResponse.json({ user: null });
      }),
      http.get(`${BASE}/api/queries`, () => {
        calls.queries = true;
        return HttpResponse.json([]);
      }),
      http.get(`${BASE}/api/embed/:token`, () => HttpResponse.json(dashboardResponse())),
    );

    renderEmbed();

    expect(await screen.findByText("Sales Overview")).toBeInTheDocument();
    expect(calls.session).toBe(false);
    expect(calls.queries).toBe(false);
  });
});
