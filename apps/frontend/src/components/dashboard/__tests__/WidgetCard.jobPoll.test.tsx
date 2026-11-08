import { describe, it, expect, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/__tests__/msw-server";
import { WidgetCard } from "../WidgetCard";
import type { Widget } from "@/types/api";

const BASE = "http://localhost:8080";

function tableWidget(overrides: Partial<Widget> = {}): Widget {
  return {
    id: 1,
    dashboard_id: 1,
    visualization_id: 10,
    visualization: {
      id: 10,
      query_id: 5,
      type: "TABLE",
      name: "Signups",
      options: {},
      created_at: "2026-01-01T00:00:00.000Z",
      updated_at: "2026-01-01T00:00:00.000Z",
      query: { id: 5, name: "Signups query", data_source_id: 1, parameter_names: [], options: {} },
    },
    query_result_data: null,
    viewer_can_read_data: true,
    text: null,
    width: 1,
    options: {},
    created_at: "2026-01-01T00:00:00.000Z",
    updated_at: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

function renderCard(widget: Widget) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <WidgetCard widget={widget} editMode={false} onDelete={() => {}} />
    </QueryClientProvider>,
  );
}

describe("WidgetCard job-poll flow", () => {
  afterEach(() => server.resetHandlers());

  it("a cache-hit result renders immediately with no job involved", async () => {
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query: "SELECT 1",
            query_hash: "h",
            data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: 7 }] },
            runtime: 0.01,
            retrieved_at: "2026-01-01T00:00:00.000Z",
          },
        }),
      ),
    );

    renderCard(tableWidget());

    expect(await screen.findByText("7")).toBeInTheDocument();
  });

  it("a cache-miss result polls the job exactly once per tick and never re-sends the execute", async () => {
    let resultFetches = 0;
    let jobPolls = 0;

    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return HttpResponse.json({ job: { id: "job-w1", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-w1`, () => {
        jobPolls += 1;
        const status = jobPolls < 2 ? 2 : 3; // started, then success
        return HttpResponse.json({
          job: { id: "job-w1", status, query_result_id: status === 3 ? 42 : null, error: null },
        });
      }),
      http.get(`${BASE}/api/query_results/42`, () =>
        HttpResponse.json({
          query_result: {
            id: 42,
            data_source_id: 1,
            query: "SELECT 1",
            query_hash: "h",
            data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: 9 }] },
            runtime: 0.02,
            retrieved_at: "2026-01-01T00:00:00.000Z",
          },
        }),
      ),
    );

    renderCard(tableWidget());

    expect(await screen.findByText(/running/i)).toBeInTheDocument();

    expect(await screen.findByText("9")).toBeInTheDocument();

    expect(resultFetches).toBe(1);
    expect(jobPolls).toBeGreaterThanOrEqual(2);

    const pollsAtDone = jobPolls;
    await new Promise((r) => setTimeout(r, 60));
    expect(jobPolls).toBe(pollsAtDone);
  });

  it("a job that ends in FAILURE renders the error once and stays put (no infinite retry loop)", async () => {
    let jobPolls = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({ job: { id: "job-w2", status: 1 } }),
      ),
      http.get(`${BASE}/api/jobs/job-w2`, () => {
        jobPolls += 1;
        return HttpResponse.json({
          job: { id: "job-w2", status: 4, query_result_id: null, error: "division by zero" },
        });
      }),
    );

    renderCard(tableWidget());

    expect(await screen.findByText("division by zero")).toBeInTheDocument();

    const pollsAtFailure = jobPolls;
    await new Promise((r) => setTimeout(r, 60));
    expect(jobPolls).toBe(pollsAtFailure);
  });

  it("a rejected query-result fetch after job success renders the error, not a stuck Running… loader", async () => {
    let jobPolls = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({ job: { id: "job-w4", status: 1 } }),
      ),
      http.get(`${BASE}/api/jobs/job-w4`, () => {
        jobPolls += 1;
        const status = jobPolls < 2 ? 2 : 3;
        return HttpResponse.json({
          job: { id: "job-w4", status, query_result_id: status === 3 ? 99 : null, error: null },
        });
      }),
      http.get(`${BASE}/api/query_results/99`, () =>
        HttpResponse.json({ message: "result expired" }, { status: 410 }),
      ),
    );

    renderCard(tableWidget());

    expect(await screen.findByText(/running/i)).toBeInTheDocument();
    expect(await screen.findByText("result expired")).toBeInTheDocument();
    expect(screen.queryByText(/running/i)).not.toBeInTheDocument();
  });

  it("a never-run query (404 on the latest-result endpoint) renders a neutral empty state, not a failure", async () => {
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({ message: "not found" }, { status: 404 }),
      ),
    );

    renderCard(tableWidget());

    const empty = await screen.findByText("No data yet — this query hasn't run.");
    expect(empty).not.toHaveClass("text-destructive");
    expect(screen.queryByText("Failed to load data")).not.toBeInTheDocument();
  });

  it("a viewer without data-source access sees the access boundary, not the never-run copy, and the doomed 404 fetch never fires", async () => {
    let resultFetches = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return HttpResponse.json({ message: "query not found" }, { status: 404 });
      }),
    );

    renderCard(tableWidget({ viewer_can_read_data: false }));

    expect(
      await screen.findByText("You don't have access to this widget's data source."),
    ).toBeInTheDocument();
    expect(screen.queryByText("No data yet — this query hasn't run.")).not.toBeInTheDocument();
    // The backend flag is authoritative; the frontend must not probe the results endpoint.
    expect(resultFetches).toBe(0);
  });

  it("a genuine server error on the latest-result endpoint still renders the destructive failure copy", async () => {
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({ message: "boom" }, { status: 500 }),
      ),
    );

    renderCard(tableWidget());

    const failure = await screen.findByText("Failed to load data");
    expect(failure).toHaveClass("text-destructive");
  });

  it("bounds the result query's gcTime instead of the default 5-minute window", async () => {
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () =>
        HttpResponse.json({
          query_result: {
            id: 1,
            data_source_id: 1,
            query: "SELECT 1",
            query_hash: "h",
            data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n: 1 }] },
            runtime: 0.01,
            retrieved_at: "2026-01-01T00:00:00.000Z",
          },
        }),
      ),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <WidgetCard widget={tableWidget()} editMode={false} onDelete={() => {}} refreshKey={0} />
      </QueryClientProvider>,
    );
    await screen.findByText("1");

    const cached = qc.getQueryCache().find({ queryKey: ["query-result", 5, undefined, 0] });
    expect(cached?.options.gcTime).toBe(30_000);
  });

  function inlineResult(n: number) {
    return HttpResponse.json({
      query_result: {
        id: n,
        data_source_id: 1,
        query: "SELECT 1",
        query_hash: "h",
        data: { columns: [{ name: "n", friendly_type: "integer" }], rows: [{ n }] },
        runtime: 0.01,
        retrieved_at: "2026-01-01T00:00:00.000Z",
      },
    });
  }

  function renderTick(qc: QueryClient, refreshKey: number) {
    return (
      <QueryClientProvider client={qc}>
        <WidgetCard widget={tableWidget()} editMode={false} onDelete={() => {}} refreshKey={refreshKey} />
      </QueryClientProvider>
    );
  }

  it("keeps the previous tick's data on screen while the next refresh resolves", async () => {
    let resultFetches = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return resultFetches === 1 ? inlineResult(1) : HttpResponse.json({ job: { id: "job-w3", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-w3`, () =>
        HttpResponse.json({ job: { id: "job-w3", status: 2, query_result_id: null, error: null } }),
      ),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(renderTick(qc, 0));
    expect(await screen.findByText("1")).toBeInTheDocument();

    rerender(renderTick(qc, 1));

    // Tile keeps showing "1": placeholder first, then the retained last-good value marked busy.
    await waitFor(() => {
      const cell = screen.getByText("1");
      expect(cell.closest('[aria-busy="true"]')).not.toBeNull();
    });
    expect(screen.queryByText(/running/i)).not.toBeInTheDocument();
  });

  it("a new refreshKey's result replaces the previous one rather than merging with it", async () => {
    let resultFetches = 0;
    let jobPolls = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return resultFetches === 1 ? inlineResult(1) : HttpResponse.json({ job: { id: "job-w6", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-w6`, () => {
        jobPolls += 1;
        const status = jobPolls < 2 ? 2 : 3;
        return HttpResponse.json({
          job: { id: "job-w6", status, query_result_id: status === 3 ? 55 : null, error: null },
        });
      }),
      http.get(`${BASE}/api/query_results/55`, () => inlineResult(2)),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(renderTick(qc, 0));
    expect(await screen.findByText("1")).toBeInTheDocument();

    rerender(renderTick(qc, 1));

    expect(await screen.findByText("2")).toBeInTheDocument();
    expect(screen.queryByText("1")).not.toBeInTheDocument();
  });

  it("an auto-refresh tick never renders the initial loader", async () => {
    let resultFetches = 0;
    let jobPolls = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return resultFetches === 1 ? inlineResult(1) : HttpResponse.json({ job: { id: "job-w7", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-w7`, () => {
        jobPolls += 1;
        const status = jobPolls < 2 ? 2 : 3;
        return HttpResponse.json({
          job: { id: "job-w7", status, query_result_id: status === 3 ? 77 : null, error: null },
        });
      }),
      http.get(`${BASE}/api/query_results/77`, () => inlineResult(2)),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(renderTick(qc, 0));
    expect(await screen.findByText("1")).toBeInTheDocument();

    rerender(renderTick(qc, 1));
    // keepPreviousData keeps old data showing, so no isLoading spinner appears.
    expect(screen.queryByRole("status", { name: "Loading" })).not.toBeInTheDocument();

    await waitFor(() => {
      expect(screen.getByText("2")).toBeInTheDocument();
      expect(screen.queryByRole("status", { name: "Loading" })).not.toBeInTheDocument();
    });
    expect(screen.queryByRole("status", { name: "Loading" })).not.toBeInTheDocument();
  });

  it("a first load still shows the loader", async () => {
    server.use(http.get(`${BASE}/api/queries/5/results`, () => inlineResult(3)));

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(renderTick(qc, 0));

    expect(screen.getByRole("status", { name: "Loading" })).toBeInTheDocument();
    expect(await screen.findByText("3")).toBeInTheDocument();
  });

  it("a failed refresh replaces the stale tile with the error, not with old numbers", async () => {
    let resultFetches = 0;
    server.use(
      http.get(`${BASE}/api/queries/5/results`, () => {
        resultFetches += 1;
        return resultFetches === 1 ? inlineResult(1) : HttpResponse.json({ job: { id: "job-w8", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-w8`, () =>
        HttpResponse.json({ job: { id: "job-w8", status: 4, query_result_id: null, error: "warehouse timeout" } }),
      ),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(renderTick(qc, 0));
    expect(await screen.findByText("1")).toBeInTheDocument();

    rerender(renderTick(qc, 1));

    expect(await screen.findByText("warehouse timeout")).toBeInTheDocument();
    expect(screen.queryByText("1")).not.toBeInTheDocument();
  });

  it("changing dashboardParams shows a loader rather than the previous params' data", async () => {
    function paramWidget(): Widget {
      return tableWidget({
        visualization: {
          id: 10,
          query_id: 5,
          type: "TABLE",
          name: "Signups",
          options: {},
          created_at: "2026-01-01T00:00:00.000Z",
          updated_at: "2026-01-01T00:00:00.000Z",
          query: { id: 5, name: "Signups query", data_source_id: 1, parameter_names: ["region"], options: {} },
        },
      });
    }
    function renderParams(qc: QueryClient, region: string) {
      return (
        <QueryClientProvider client={qc}>
          <WidgetCard
            widget={paramWidget()}
            editMode={false}
            onDelete={() => {}}
            dashboardParams={{ region }}
          />
        </QueryClientProvider>
      );
    }

    let jobPolls = 0;
    server.use(
      http.post(`${BASE}/api/queries/5/results`, async ({ request }) => {
        const body = (await request.json()) as { parameters?: { region?: string } };
        // "us" completes; "eu" hands back a job that never does, holding a param-change refresh
        // open.
        return HttpResponse.json({ job: { id: body.parameters?.region === "eu" ? "job-eu" : "job-us", status: 1 } });
      }),
      http.get(`${BASE}/api/jobs/job-us`, () => {
        jobPolls += 1;
        const status = jobPolls < 2 ? 2 : 3;
        return HttpResponse.json({
          job: { id: "job-us", status, query_result_id: status === 3 ? 1 : null, error: null },
        });
      }),
      http.get(`${BASE}/api/query_results/1`, () => inlineResult(1)),
      http.get(`${BASE}/api/jobs/job-eu`, () =>
        HttpResponse.json({ job: { id: "job-eu", status: 2, query_result_id: null, error: null } }),
      ),
    );

    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const { rerender } = render(renderParams(qc, "us"));
    expect(await screen.findByText("1")).toBeInTheDocument();

    rerender(renderParams(qc, "eu"));

    // The previous question's answer must not reappear once the new job supersedes it; only the
    // running state.
    await waitFor(() => expect(screen.getByText(/running/i)).toBeInTheDocument());
    expect(screen.queryByText("1")).not.toBeInTheDocument();
  });
});
