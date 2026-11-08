import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { DataSourceDetailPage } from "@/pages/DataSourceDetailPage";
import { AuthProvider } from "@/lib/auth";
import { api } from "@/lib/api";

const BASE = "http://localhost:8080";

function makeDataSource(overrides: Record<string, unknown> = {}) {
  return {
    id: 7,
    name: "Prod Postgres",
    type: "pg",
    type_display_name: "PostgreSQL",
    schema: null,
    schema_refreshed_at: null,
    paused: false,
    pause_reason: null,
    health_state: "unknown",
    health_error: null,
    health_checked_at: null,
    created_at: "2025-01-01T00:00:00Z",
    updated_at: "2025-01-01T00:00:00Z",
    caller: { object_role: "editor", can_edit: true, can_delete: true },
    ...overrides,
  };
}

// Route /data-sources/:id gives useParams id=7; the /data-sources sentinel asserts the delete
// redirect.
function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <AuthProvider>
        <MemoryRouter initialEntries={["/data-sources/7"]}>
          <Routes>
            <Route path="/data-sources/:id" element={<DataSourceDetailPage />} />
            <Route path="/data_sources" element={<p>Data sources list</p>} />
          </Routes>
        </MemoryRouter>
      </AuthProvider>
    </QueryClientProvider>,
  );
}

describe("DataSourceDetailPage", () => {
  beforeEach(() => {
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () => HttpResponse.json(makeDataSource())),
      http.get(`${BASE}/api/data_sources/7/schema`, () =>
        HttpResponse.json({ schema: { tables: [] } }),
      ),
      http.get(`${BASE}/api/data_sources/7/groups`, () => HttpResponse.json([])),
    );
  });

  it("renders the data source name and On indicator", async () => {
    renderPage();
    expect(await screen.findByRole("heading", { name: "Prod Postgres" })).toBeInTheDocument();
    expect(screen.getByText("On · Not checked yet")).toBeInTheDocument();
  });

  it("titles the tab with the data source's own name, not a static section noun", async () => {
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });
    expect(document.title).toBe("Prod Postgres · arcodash");
  });

  it("links to the connection-health docs under the health indicator", async () => {
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });
    expect(
      screen.getByRole("link", { name: /learn more about connection health/i }),
    ).toHaveAttribute("href", "https://www.arcodash.com/docs/connecting-data/overview#connection-health");
  });

  it("test connection success shows a green success message", async () => {
    server.use(
      http.post(`${BASE}/api/data_sources/test`, () =>
        HttpResponse.json({ message: "Connection OK" }),
      ),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: /test connection/i }));

    const msg = await screen.findByText("Connection OK");
    expect(msg).toBeInTheDocument();
    expect(msg.className).toContain("text-green-600");
    expect(msg.querySelector("svg")).toBeInTheDocument();
  });

  it("test connection failure shows an error message without crashing", async () => {
    server.use(
      http.post(`${BASE}/api/data_sources/test`, () =>
        HttpResponse.json({ message: "auth failed: bad password" }, { status: 400 }),
      ),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: /test connection/i }));

    const msg = await screen.findByText("auth failed: bad password");
    expect(msg.className).toContain("text-destructive");
    expect(msg.querySelector("svg")).toBeInTheDocument();
    expect(screen.getByRole("heading", { name: "Prod Postgres" })).toBeInTheDocument();
  });

  it("a failing test connection refreshes the health badge and banner without a reload", async () => {
    let tested = false;
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json(
          tested
            ? makeDataSource({
                health_state: "failing",
                health_error: "auth failed: bad password",
                health_checked_at: "2026-01-02T00:00:00Z",
              })
            : makeDataSource(),
        ),
      ),
      http.post(`${BASE}/api/data_sources/test`, () => {
        tested = true;
        return HttpResponse.json({ message: "auth failed: bad password" }, { status: 400 });
      }),
    );

    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });

    await userEvent.click(screen.getByRole("button", { name: /test connection/i }));

    await screen.findByRole("button", { name: /test connection/i });
    expect(await screen.findByText("On · Unreachable")).toBeInTheDocument();
    expect(screen.getByText("Can't connect")).toBeInTheDocument();
    expect(screen.getByText("Last checked", { exact: false })).toBeInTheDocument();
    expect(screen.getAllByText("auth failed: bad password")).toHaveLength(2);
  });

  it("turning off calls the pause endpoint and shows the Off indicator", async () => {
    let getCount = 0;
    let pauseCalled = false;
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () => {
        getCount += 1;
        return HttpResponse.json(
          makeDataSource(pauseCalled ? { paused: true, pause_reason: null } : {}),
        );
      }),
      http.post(`${BASE}/api/data_sources/7/pause`, () => {
        pauseCalled = true;
        return HttpResponse.json(makeDataSource({ paused: true }));
      }),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: /^turn off$/i }));

    expect(await screen.findByText("Off")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /turn on/i })).toBeInTheDocument();
    expect(pauseCalled).toBe(true);
    expect(getCount).toBeGreaterThan(1);
  });

  it("turning on calls the resume endpoint and reverses to On", async () => {
    let resumeCalled = false;
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json(
          makeDataSource(resumeCalled ? {} : { paused: true, pause_reason: "manual" }),
        ),
      ),
      http.post(`${BASE}/api/data_sources/7/resume`, () => {
        resumeCalled = true;
        return HttpResponse.json(makeDataSource());
      }),
    );

    renderPage();

    await userEvent.click(await screen.findByRole("button", { name: /turn on/i }));

    expect(await screen.findByText("On · Not checked yet")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^turn off$/i })).toBeInTheDocument();
    expect(resumeCalled).toBe(true);
  });

  it("shows an inline error when turning off fails, instead of failing silently", async () => {
    server.use(
      http.post(`${BASE}/api/data_sources/7/pause`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: /^turn off$/i }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /^turn off$/i })).toBeInTheDocument();
  });

  it("shows an inline error when turning on fails, instead of failing silently", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json(makeDataSource({ paused: true, pause_reason: "manual" })),
      ),
      http.post(`${BASE}/api/data_sources/7/resume`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: /turn on/i }));

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });

  it("schema browser renders tables from the schema endpoint", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7/schema`, () =>
        HttpResponse.json({
          schema: {
            tables: [
              {
                name: "users",
                columns: [
                  { name: "id", type_name: "integer" },
                  { name: "email", type_name: "text" },
                ],
              },
              { name: "orders", columns: [{ name: "id", type_name: "integer" }, { name: "total" }] },
            ],
          },
        }),
      ),
    );

    renderPage();

    expect(await screen.findByRole("button", { name: /users/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /orders/i })).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /users/i }));
    expect(await screen.findByRole("button", { name: /^email/ })).toBeInTheDocument();
  });

  it("schema browser shows an empty state when there are no tables", async () => {
    renderPage();
    expect(await screen.findByText(/no tables found/i)).toBeInTheDocument();
  });

  it("schema browser shows each column's source type", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7/schema`, () =>
        HttpResponse.json({
          schema: {
            tables: [
              {
                name: "users",
                columns: [
                  { name: "id", type_name: "integer" },
                  { name: "email", type_name: "text" },
                ],
              },
              { name: "orders", columns: [{ name: "id", type_name: "integer" }, { name: "total" }] },
            ],
          },
        }),
      ),
    );

    renderPage();
    await userEvent.click(await screen.findByRole("button", { name: /users/i }));
    expect(await screen.findByRole("button", { name: /^email\s+text$/ })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /orders/i }));
    expect(await screen.findByRole("button", { name: /^total$/ })).toBeInTheDocument();
  });

  it("shows Edit and Delete on the header for an editable data source, and Edit links to the edit route", async () => {
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });
    expect(screen.getByRole("link", { name: /edit/i })).toHaveAttribute(
      "href",
      "/data_sources/7/edit",
    );
    expect(screen.getByRole("button", { name: /delete/i })).toBeInTheDocument();
  });

  it("hides Edit and Delete when the caller can't edit", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json(makeDataSource({ caller: { object_role: "viewer", can_edit: false, can_delete: false } })),
      ),
    );
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });
    expect(screen.queryByRole("link", { name: /edit/i })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /delete/i })).not.toBeInTheDocument();
  });

  it("Delete asks for confirmation, then calls the delete endpoint and redirects to the list", async () => {
    let deleteCalled = false;
    server.use(
      http.delete(`${BASE}/api/data_sources/7`, () => {
        deleteCalled = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });

    await userEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/permanently delete/i)).toBeInTheDocument();
    expect(deleteCalled).toBe(false);

    await userEvent.click(within(dialog).getByRole("button", { name: /^delete$/i }));

    expect(await screen.findByText("Data sources list")).toBeInTheDocument();
    expect(deleteCalled).toBe(true);
  });

  it("surfaces a delete conflict inside the still-open dialog", async () => {
    server.use(
      http.delete(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json({ message: "3 queries still use this data source" }, { status: 409 }),
      ),
    );
    renderPage();
    await screen.findByRole("heading", { name: "Prod Postgres" });

    await userEvent.click(screen.getByRole("button", { name: /delete/i }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: /^delete$/i }));

    expect(
      await within(dialog).findByText("3 queries still use this data source"),
    ).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("group access lists an unrestricted group as reaching every data source and a restricted group as toggleable", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7/groups`, () =>
        HttpResponse.json([
          { group_id: 1, group_name: "everyone", restricted: false, has_access: true },
          { group_id: 2, group_name: "analysts", restricted: true, has_access: false },
        ]),
      ),
    );
    renderPage();

    expect(await screen.findByText("everyone")).toBeInTheDocument();
    expect(screen.getByText("analysts")).toBeInTheDocument();
    expect(screen.getAllByText("All data sources")).toHaveLength(1);
    expect(screen.getByRole("switch")).not.toBeChecked();
  });

  it("toggling a restricted group's access on calls the group-keyed assign endpoint", async () => {
    let assignedGroupId: number | null = null;
    server.use(
      http.get(`${BASE}/api/data_sources/7/groups`, () =>
        HttpResponse.json([
          { group_id: 2, group_name: "analysts", restricted: true, has_access: false },
        ]),
      ),
      http.post(`${BASE}/api/groups/2/data_sources`, async ({ request }) => {
        const body = (await request.json()) as { data_source_id: number };
        assignedGroupId = body.data_source_id;
        return HttpResponse.json({ ok: true });
      }),
    );
    renderPage();

    const toggle = await screen.findByRole("switch");
    await userEvent.click(toggle);

    expect(assignedGroupId).toBe(7);
  });

  it("shows an inline error when granting group access fails, instead of failing silently", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7/groups`, () =>
        HttpResponse.json([
          { group_id: 2, group_name: "analysts", restricted: true, has_access: false },
        ]),
      ),
      http.post(`${BASE}/api/groups/2/data_sources`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );
    renderPage();

    const toggle = await screen.findByRole("switch");
    await userEvent.click(toggle);

    expect(await screen.findByText("database unavailable")).toBeInTheDocument();
  });
});

describe("DataSourceDetailPage bad-id handling", () => {
  function renderAt(path: string) {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <AuthProvider>
          <MemoryRouter initialEntries={[path]}>
            <Routes>
              <Route path="/data-sources/:id" element={<DataSourceDetailPage />} />
              <Route path="/data_sources" element={<p>Data sources list</p>} />
            </Routes>
          </MemoryRouter>
        </AuthProvider>
      </QueryClientProvider>,
    );
  }

  it("a non-numeric route param never calls the API and shows the not-found card", async () => {
    const getDataSource = vi.spyOn(api, "getDataSource");
    server.use(
      http.get(`${BASE}/api/data_sources/foo`, () =>
        HttpResponse.json({ message: "Invalid URL: Cannot parse `foo` to a `i64`" }, { status: 400 }),
      ),
    );

    renderAt("/data-sources/foo");

    expect(await screen.findByText("Data source not found")).toBeInTheDocument();
    expect(
      screen.getByText("This data source doesn't exist or you don't have access to it."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Cannot parse/i)).not.toBeInTheDocument();
    expect(getDataSource).not.toHaveBeenCalled();

    await userEvent.click(screen.getByRole("link", { name: /back to data sources/i }));
    expect(await screen.findByText("Data sources list")).toBeInTheDocument();

    getDataSource.mockRestore();
  });

  it("a numeric but nonexistent id (404) shows the same not-found card", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/999`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
    );

    renderAt("/data-sources/999");

    expect(await screen.findByText("Data source not found")).toBeInTheDocument();
  });

  it("a real server error (500) is not mislabeled as not-found", async () => {
    server.use(
      http.get(`${BASE}/api/data_sources/7`, () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );

    renderAt("/data-sources/7");

    expect(await screen.findByText("Couldn't load this data source")).toBeInTheDocument();
    expect(screen.getByText("database unavailable")).toBeInTheDocument();
    expect(screen.queryByText("Data source not found")).not.toBeInTheDocument();
  });
});
