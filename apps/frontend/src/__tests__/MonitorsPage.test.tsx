import { describe, it, expect, beforeAll, afterAll, vi } from "vitest";
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import { mockViewport } from "./mockViewport";
import { MonitorsPage } from "@/pages/MonitorsPage";

const BASE = "http://localhost:8080";

const authState = vi.hoisted(() => ({ clientConfig: null as Record<string, boolean> | null }));
vi.mock("@/lib/auth", () => ({ useAuth: () => ({ clientConfig: authState.clientConfig }) }));

const MONITOR = {
  id: 1,
  name: "Too many signups",
  query: { id: 7, name: "Signups query" },
  state: "ok",
  last_triggered_at: null,
  user: { id: 1, name: "Test User", email: "test@example.com" },
  visibility: "workspace",
  caller: { object_role: "owner", can_edit: true, can_delete: true },
};

// setup.ts stubs matchMedia to matches:false globally, which useListView reads as "mobile" and
// forces the cards renderer — pin desktop for the whole file so the default-table assertions
// below hold; the view toggle itself is covered in its own describe block.
let restoreViewport: () => void;
beforeAll(() => {
  restoreViewport = mockViewport(true);
});
afterAll(() => {
  restoreViewport();
});

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <MonitorsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("MonitorsPage", () => {
  it("hides the Query and Last Triggered columns below a container width, keeping Name/State visible", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));

    renderPage();
    await screen.findByText("Too many signups");

    const queryCell = screen.getByText("Signups query");
    expect(queryCell.closest("td")).toHaveClass("hidden", "@min-[54rem]/list:table-cell");
    const nameCell = screen.getByText("Too many signups");
    expect(nameCell.closest("td")).not.toHaveClass("hidden");
  });

  it("shows an EmptyState with a Create Your First Monitor action that navigates to /monitors/new", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])));

    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <MemoryRouter initialEntries={["/monitors"]}>
          <Routes>
            <Route path="/monitors" element={<MonitorsPage />} />
            <Route path="/monitors/new" element={<div>New Monitor Page</div>} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    await screen.findByText("Set up your first monitor");
    await userEvent.click(screen.getByRole("button", { name: /create your first monitor/i }));

    expect(await screen.findByText("New Monitor Page")).toBeInTheDocument();
  });

  it("links the query cell to the query's page", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));

    renderPage();
    const queryLink = await screen.findByRole("link", { name: "Signups query" });
    expect(queryLink).toHaveAttribute("href", "/queries/7");
  });

  it("shows the owner and visibility chip on each row", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));

    renderPage();
    expect(await screen.findByText("Too many signups")).toBeInTheDocument();
    expect(screen.getByText("Test User")).toBeInTheDocument();
    expect(screen.getByText("Everyone")).toBeInTheDocument();
  });

  it("wires the scope toggle to the ?filter= param", async () => {
    const seenFilters: (string | null)[] = [];
    server.use(
      http.get(`${BASE}/api/monitors`, ({ request }) => {
        seenFilters.push(new URL(request.url).searchParams.get("filter"));
        return HttpResponse.json([MONITOR]);
      }),
    );

    renderPage();
    expect(await screen.findByText("Too many signups")).toBeInTheDocument();
    expect(seenFilters).toContain(null);

    fireEvent.click(screen.getByRole("button", { name: "By me" }));
    await waitFor(() => expect(seenFilters).toContain("by-me"));

    fireEvent.click(screen.getByRole("button", { name: "Shared with me" }));
    await waitFor(() => expect(seenFilters).toContain("shared-with-me"));
  });

  it("renders the page heading above the Monitors/Destinations tab bar, not beneath it", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));
    renderPage();
    await screen.findByText("Too many signups");

    const heading = screen.getByRole("heading", { name: "Monitors" });
    const tab = screen.getByRole("tab", { name: "Monitors" });
    // DOCUMENT_POSITION_FOLLOWING (4) means `tab` comes after `heading` in the DOM.
    expect(heading.compareDocumentPosition(tab) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it("filters the list by name and by the watched query's name, client-side", async () => {
    const other = { ...MONITOR, id: 2, name: "Other monitor", query: { id: 8, name: "Other query" } };
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR, other])));
    renderPage();
    await screen.findByText("Too many signups");
    expect(screen.getByText("Other monitor")).toBeInTheDocument();

    await userEvent.type(screen.getByPlaceholderText("Search monitors…"), "signups");

    await waitFor(() => expect(screen.queryByText("Other monitor")).not.toBeInTheDocument());
    expect(screen.getByText("Too many signups")).toBeInTheDocument();
  });
});

describe("MonitorsPage no-subscribers badge", () => {
  it("shows the badge once subscriptions resolve to an empty list", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.get(`${BASE}/api/monitors/1/subscriptions`, () => HttpResponse.json([])),
    );
    renderPage();
    await screen.findByText("Too many signups");
    expect(await screen.findByText("No subscribers")).toBeInTheDocument();
  });

  it("never shows the badge for a monitor that has subscribers", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.get(`${BASE}/api/monitors/1/subscriptions`, () =>
        HttpResponse.json([{ id: 1, destination_id: 1, destination_name: "Slack", destination_type: "slack" }]),
      ),
    );
    renderPage();
    await screen.findByText("Too many signups");
    await waitFor(() => expect(screen.queryByText("No subscribers")).not.toBeInTheDocument());
  });

  it("never shows a false positive while subscriptions are still loading", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.get(`${BASE}/api/monitors/1/subscriptions`, async () => {
        await new Promise((r) => setTimeout(r, 50));
        return HttpResponse.json([]);
      }),
    );
    renderPage();
    await screen.findByText("Too many signups");
    expect(screen.queryByText("No subscribers")).not.toBeInTheDocument();
    expect(await screen.findByText("No subscribers")).toBeInTheDocument();
  });
});

describe("MonitorsPage row actions", () => {
  it("exposes a Delete action in the table view, gated on caller.can_edit", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));
    renderPage();
    await screen.findByText("Too many signups");
    expect(screen.getByRole("button", { name: "Delete monitor" })).toBeInTheDocument();
  });

  it("hides Delete for a monitor the caller can't edit", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () =>
        HttpResponse.json([{ ...MONITOR, caller: { object_role: "viewer", can_edit: false, can_delete: false } }]),
      ),
    );
    renderPage();
    await screen.findByText("Too many signups");
    expect(screen.queryByRole("button", { name: "Delete monitor" })).not.toBeInTheDocument();
  });

  it("keeps Delete available after switching to the card view", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));
    renderPage();
    await screen.findByText("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: "Card view" }));

    expect(screen.queryAllByRole("row")).toHaveLength(0);
    expect(screen.getByRole("button", { name: "Delete monitor" })).toBeInTheDocument();
  });

  it("deleting a monitor opens a confirm dialog and calls DELETE on confirm", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.delete(`${BASE}/api/monitors/1`, () => new HttpResponse(null, { status: 204 })),
    );
    renderPage();
    await screen.findByText("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: "Delete monitor" }));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/Too many signups/)).toBeInTheDocument();

    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("a failed delete shows the reason inside the confirm dialog", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.delete(`${BASE}/api/monitors/1`, () =>
        HttpResponse.json({ message: "Monitor has active subscriptions" }, { status: 409 }),
      ),
    );
    renderPage();
    await screen.findByText("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: "Delete monitor" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));

    expect(await within(dialog).findByText("Monitor has active subscriptions")).toBeInTheDocument();
  });

  it("cancelling after a failed delete clears the error", async () => {
    server.use(
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])),
      http.delete(`${BASE}/api/monitors/1`, () =>
        HttpResponse.json({ message: "Monitor has active subscriptions" }, { status: 409 }),
      ),
    );
    renderPage();
    await screen.findByText("Too many signups");

    await userEvent.click(screen.getByRole("button", { name: "Delete monitor" }));
    let dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Delete" }));
    await within(dialog).findByText("Monitor has active subscriptions");

    await userEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await userEvent.click(screen.getByRole("button", { name: "Delete monitor" }));
    dialog = await screen.findByRole("dialog");
    expect(within(dialog).queryByText("Monitor has active subscriptions")).not.toBeInTheDocument();
  });

  it("the monitor name is a focusable link that navigates on Enter", async () => {
    server.use(http.get(`${BASE}/api/monitors`, () => HttpResponse.json([MONITOR])));
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <MemoryRouter initialEntries={["/monitors"]}>
          <Routes>
            <Route path="/monitors" element={<MonitorsPage />} />
            <Route path="/monitors/:id" element={<div>Monitor Detail Page</div>} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );
    await screen.findByText("Too many signups");

    const link = screen.getByRole("link", { name: "Too many signups" });
    link.focus();
    expect(link).toHaveFocus();
    await userEvent.keyboard("{Enter}");

    expect(await screen.findByText("Monitor Detail Page")).toBeInTheDocument();
  });
});

describe("MonitorsPage Add with AI entry", () => {
  const mockSession = (permissions: string[]) =>
    server.use(
      http.get(`${BASE}/api/session`, () => HttpResponse.json({ permissions, is_admin: false })),
      http.get(`${BASE}/api/monitors`, () => HttpResponse.json([])),
    );

  it("shows the header button and footer link when AI and permissions allow", async () => {
    authState.clientConfig = { ai_sql_generation: true, ai_monitor_suggestions: true };
    mockSession(["create_monitor", "create_query"]);
    renderPage();
    expect(await screen.findByTestId("monitor-ask-open")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Describe it to AI" })).toBeInTheDocument();
  });

  it("hides the entry when AI is off", async () => {
    authState.clientConfig = { ai_sql_generation: false, ai_monitor_suggestions: true };
    mockSession(["create_monitor", "create_query"]);
    renderPage();
    await screen.findByText("Set up your first monitor");
    expect(screen.queryByTestId("monitor-ask-open")).toBeNull();
    expect(screen.queryByRole("button", { name: "Describe it to AI" })).toBeNull();
  });

  it("hides the entry without create_query", async () => {
    authState.clientConfig = { ai_sql_generation: true, ai_monitor_suggestions: true };
    mockSession(["create_monitor"]);
    renderPage();
    await screen.findByText("Set up your first monitor");
    expect(screen.queryByTestId("monitor-ask-open")).toBeNull();
  });
});
