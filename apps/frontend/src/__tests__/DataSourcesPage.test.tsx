import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { DataSourcesPage } from "@/pages/DataSourcesPage";
import { DataSourceDetailPage } from "@/pages/DataSourceDetailPage";
import { AuthProvider } from "@/lib/auth";

const BASE = "http://localhost:8080";

function makeDataSource(overrides: Record<string, unknown> = {}) {
  return {
    id: 7,
    name: "Prod Postgres",
    type: "pg",
    schema: null,
    schema_refreshed_at: null,
    paused: false,
    pause_reason: null,
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
    caller: { object_role: "editor", can_edit: true, can_delete: true },
    ...overrides,
  };
}

function sessionHandler(opts: { isAdmin: boolean; permissions: string[] }) {
  return http.get(`${BASE}/api/session`, () =>
    HttpResponse.json({
      user: { id: 1, name: "Test User", email: "test@example.com" },
      org: { id: 1, name: "Test Org", slug: "test-org" },
      permissions: opts.permissions,
      is_admin: opts.isAdmin,
    }),
  );
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/data_sources"]}>
        <DataSourcesPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("DataSourcesPage", () => {
  it("disables the create button and shows the inline reason when can_create is false", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: ["create_data_source"] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [makeDataSource()], can_create: false }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    // `aria-label` overrides the button's accessible name to the short reason when disabled
    // (screen-reader parity with the `title`), so query by that, not the visible "+ New data
    // source" label.
    const button = await screen.findByRole("button", {
      name: "Your access is limited to specific data sources — ask an admin to add more.",
    });
    expect(button).toBeDisabled();
    expect(button).toHaveAttribute(
      "title",
      "Your access is limited to specific data sources — ask an admin to add more.",
    );
    expect(
      screen.getByText(
        "You can't add a data source right now because your access is limited to specific data sources. Ask a workspace admin to widen your access.",
      ),
    ).toBeInTheDocument();
  });

  it("shows an enabled create button with no inline reason when can_create is true", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: ["create_data_source"] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [makeDataSource()], can_create: true }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    const button = await screen.findByRole("button", { name: "+ New data source" });
    expect(button).not.toBeDisabled();
    expect(
      screen.queryByText(
        "You can't add a data source right now because your access is limited to specific data sources. Ask a workspace admin to widen your access.",
      ),
    ).not.toBeInTheDocument();
  });

  it("hides the create button entirely when the user lacks create_data_source", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: [] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [makeDataSource()], can_create: false }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    await screen.findByText("Prod Postgres");
    expect(screen.queryByRole("button", { name: "+ New data source" })).not.toBeInTheDocument();
    // Heading uses the shared PageHeader treatment, not a hardcoded text-3xl h1.
    const heading = screen.getByRole("heading", { name: "Data Sources" });
    expect(heading.className).toContain("text-2xl");
    expect(heading.className).toContain("font-bold");
  });

  it("empty-state: create-eligible viewer sees the connect-your-first-data-source copy", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: ["create_data_source"] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [], can_create: true }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    expect(await screen.findByText("Connect your first data source")).toBeInTheDocument();
    expect(
      screen.getByText("Hook up a database or API, then query it and monitor its data quality."),
    ).toBeInTheDocument();
  });

  it("empty-state: create-blocked viewer sees the ask-an-admin copy, not the connect-first copy", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: ["create_data_source"] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [], can_create: false }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    expect(await screen.findByText("No data sources available")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Your access is limited to specific data sources — ask a workspace admin to grant access to the ones you need.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText("Connect your first data source")).not.toBeInTheDocument();
  });

  it("empty-state: non-creator viewer (no capability) also sees the ask-an-admin copy", async () => {
    server.use(
      sessionHandler({ isAdmin: false, permissions: [] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({ data_sources: [], can_create: false }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
    );

    renderPage();

    expect(await screen.findByText("No data sources available")).toBeInTheDocument();
  });

  it("a successful test connection from the detail page refreshes the list card on navigating back, without a reload", async () => {
    let tested = false;
    server.use(
      sessionHandler({ isAdmin: false, permissions: ["create_data_source"] }),
      http.get(`${BASE}/api/data_sources`, () =>
        HttpResponse.json({
          data_sources: [makeDataSource(tested ? { health_state: "ok" } : { health_state: "unknown" })],
          can_create: true,
        }),
      ),
      http.get(`${BASE}/api/data_sources/types`, () => HttpResponse.json([])),
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json(makeDataSource(tested ? { health_state: "ok" } : { health_state: "unknown" })),
      ),
      http.get(`${BASE}/api/data_sources/7/schema`, () =>
        HttpResponse.json({ schema: { tables: [] } }),
      ),
      http.get(`${BASE}/api/data_sources/7/groups`, () => HttpResponse.json([])),
      http.post(`${BASE}/api/data_sources/test`, () => {
        tested = true;
        return HttpResponse.json({ message: "Connection OK" });
      }),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={["/data_sources"]}>
            <Routes>
              <Route path="/data_sources" element={<DataSourcesPage />} />
              <Route path="/data_sources/:id" element={<DataSourceDetailPage />} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );

    expect(await screen.findByText("On · Not checked yet")).toBeInTheDocument();

    await userEvent.click(await screen.findByRole("link", { name: "Prod Postgres" }));
    await userEvent.click(await screen.findByRole("button", { name: /test connection/i }));
    await screen.findByText("Connection OK");

    await userEvent.click(await screen.findByRole("button", { name: /back to data sources/i }));

    // The list card reflects the fresh "Healthy" status immediately on return — no reload.
    expect(await screen.findByText("On · Healthy")).toBeInTheDocument();
    expect(screen.queryByText("On · Not checked yet")).not.toBeInTheDocument();
  });
});
