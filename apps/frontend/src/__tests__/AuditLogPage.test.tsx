import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import AuditLogPage from "@/pages/AuditLogPage";

const BASE = "http://localhost:8080";

function makeEvents(count: number, offset = 0) {
  return Array.from({ length: count }, (_, i) => ({
    id: offset + i + 1,
    action: "query.execute",
    object_type: "Query",
    object_id: i + 1,
    user_id: 1,
    created_at: new Date().toISOString(),
  }));
}

function jsonWithTotal<T extends Record<string, unknown> | Record<string, unknown>[]>(body: T, total: number) {
  return HttpResponse.json(body, { headers: { "X-Total-Count": String(total) } });
}

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <AuditLogPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("AuditLogPage", () => {
  it("renders events table", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () => jsonWithTotal(makeEvents(3), 3)),
    );

    renderPage();

    const cells = await screen.findAllByText("Query Execute");
    expect(cells).toHaveLength(3);
  });

  it("renders a readable label for both a settled dotted action and a legacy bare one", async () => {
    // The backend translates legacy actions, but the formatter must render either shape the same
    // way.
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 10,
              action: "dashboard.share_token_minted",
              object_type: "dashboard",
              object_id: 1,
              object: null,
              user_id: 1,
              user_name: "Priya Shah",
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
            {
              id: 11,
              action: "monitor_acknowledged",
              object_type: "monitor",
              object_id: 2,
              object: null,
              user_id: 1,
              user_name: "Priya Shah",
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
          ],
          2,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText("Dashboard Share Token Minted")).toBeInTheDocument();
    expect(screen.getByText("Monitor Acknowledged")).toBeInTheDocument();
  });

  it("next/prev pagination changes the page and shows the total count", async () => {
    const capturedUrls: string[] = [];

    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        capturedUrls.push(new URL(request.url).search);
        return jsonWithTotal(makeEvents(50), 120);
      }),
    );

    renderPage();

    await screen.findByText(/Page 1 of 3/i);
    expect(screen.getByText("120 events")).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /next/i }));
    await screen.findByText(/Page 2 of 3/i);

    await userEvent.click(screen.getByRole("button", { name: /prev/i }));
    await screen.findByText(/Page 1 of 3/i);

    expect(capturedUrls.some((u) => u.includes("page=2"))).toBe(true);
  });

  it("renders resolved user names and links objects that have a page", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 1,
              action: "data_source.created",
              object_type: "data_source",
              object_id: 185,
              object: { name: "Prod Warehouse", path: "/data_sources/185" },
              user_id: 146,
              user_name: "Priya Shah",
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
            {
              id: 2,
              action: "monitor.state_changed",
              object_type: "monitor",
              object_id: 182,
              object: { name: "Signups Alert", path: null },
              user_id: null,
              user_name: null,
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
          ],
          2,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText("Priya Shah")).toBeInTheDocument();
    expect(screen.getByText("Data Source Created")).toBeInTheDocument();
    const link = screen.getByRole("link", { name: "Prod Warehouse" });
    expect(link).toHaveAttribute("href", "/data_sources/185");
    expect(screen.getByText("Data source")).toBeInTheDocument();
    expect(screen.getByText("Signups Alert")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Signups Alert" })).not.toBeInTheDocument();
    expect(screen.getByText("Monitor")).toBeInTheDocument();
    expect(screen.getByText("Monitor State Changed")).toBeInTheDocument();
  });

  it("shows the owning query's name for a query_result execution and links to it", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 3,
              action: "execute_query",
              object_type: "query_result",
              object_id: 245,
              object: { name: "Weekly Revenue", path: "/queries/9" },
              user_id: 1,
              user_name: "Priya Shah",
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
          ],
          1,
        ),
      ),
    );

    renderPage();

    const link = await screen.findByRole("link", { name: "Weekly Revenue" });
    expect(link).toHaveAttribute("href", "/queries/9");
    expect(screen.getByText("Query result")).toBeInTheDocument();
    expect(screen.queryByText(/query_result#245/)).not.toBeInTheDocument();
  });

  it("falls back to a deleted marker when the object can't be resolved", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 4,
              action: "monitor_deleted",
              object_type: "monitor",
              object_id: 99,
              object: null,
              user_id: 1,
              user_name: "Priya Shah",
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
          ],
          1,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText(/Monitor #99/)).toBeInTheDocument();
    expect(screen.getByText("(deleted)")).toBeInTheDocument();
  });

  it("shows an ad-hoc query label instead of `Query #null` when object_id is absent", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 7,
              action: "execute_query_failed",
              object_type: "query",
              object_id: null,
              object: null,
              user_id: 1,
              user_name: "Priya Shah",
              additional_properties: { error: "connection refused" },
              created_at: new Date().toISOString(),
            },
          ],
          1,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText("An ad-hoc query")).toBeInTheDocument();
    expect(screen.queryByText(/#null/)).not.toBeInTheDocument();
  });

  it("shows the attempted email and source IP for a failed login, never blank", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 5,
              action: "user.login_failed",
              object_type: null,
              object_id: null,
              object: null,
              user_id: null,
              user_name: null,
              additional_properties: { attempted_email: "nobody@example.com", ip: "203.0.113.5" },
              created_at: new Date().toISOString(),
            },
          ],
          1,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText("nobody@example.com")).toBeInTheDocument();
    expect(screen.getByText("from 203.0.113.5")).toBeInTheDocument();
  });

  it("never renders a blank object cell for a failed login missing email/ip", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () =>
        jsonWithTotal(
          [
            {
              id: 6,
              action: "user.login_failed",
              object_type: null,
              object_id: null,
              object: null,
              user_id: null,
              user_name: null,
              additional_properties: {},
              created_at: new Date().toISOString(),
            },
          ],
          1,
        ),
      ),
    );

    renderPage();

    expect(await screen.findByText("Unknown account")).toBeInTheDocument();
    expect(screen.getByText("from an unknown address")).toBeInTheDocument();
  });

  it("action filter triggers a new fetch with the action param", async () => {
    const capturedUrls: string[] = [];

    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        capturedUrls.push(new URL(request.url).search);
        return jsonWithTotal(makeEvents(1), 1);
      }),
    );

    renderPage();

    await screen.findByText(/Page 1/i);

    const filterInput = screen.getByPlaceholderText(/filter by action/i);
    await userEvent.type(filterInput, "login");

    await waitFor(() => {
      expect(capturedUrls.some((u) => u.includes("action=login"))).toBe(true);
    });
  });

  it("date filters trigger a new fetch with from/to params", async () => {
    const capturedUrls: string[] = [];

    server.use(
      http.get(`${BASE}/api/events`, ({ request }) => {
        capturedUrls.push(new URL(request.url).search);
        return jsonWithTotal(makeEvents(1), 1);
      }),
    );

    renderPage();
    await screen.findByText(/Page 1/i);

    await userEvent.type(screen.getByLabelText("From date"), "2026-08-01");
    await userEvent.type(screen.getByLabelText("To date"), "2026-08-06");

    await waitFor(() => {
      expect(capturedUrls.some((u) => u.includes("from=2026-08-01"))).toBe(true);
      expect(capturedUrls.some((u) => u.includes("to=2026-08-06"))).toBe(true);
    });
  });

  it("shows an empty state when a filter yields no events", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () => jsonWithTotal([], 0)),
    );

    renderPage();

    const filterInput = screen.getByPlaceholderText(/filter by action/i);
    await userEvent.type(filterInput, "nonexistent");

    expect(await screen.findByText("No events in this range")).toBeInTheDocument();
  });

  it("a failed events query shows an error and a retry, not '0 events'", async () => {
    server.use(
      http.get(`${BASE}/api/events`, () => HttpResponse.json({ message: "boom" }, { status: 500 })),
    );

    renderPage();

    expect(await screen.findByText("boom")).toBeInTheDocument();
    expect(screen.queryByText(/0 events/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("retry refetches", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/events`, () => {
        calls += 1;
        if (calls === 1) return HttpResponse.json({ message: "boom" }, { status: 500 });
        return jsonWithTotal(makeEvents(1), 1);
      }),
    );

    renderPage();

    await screen.findByRole("button", { name: /try again/i });
    await userEvent.click(screen.getByRole("button", { name: /try again/i }));

    await screen.findByText("1 events");
    expect(calls).toBe(2);
  });
});
