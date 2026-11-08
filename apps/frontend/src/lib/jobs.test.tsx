import { describe, it, expect, afterEach } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "../__tests__/msw-server";
import { usePollJob } from "./jobs";

const BASE = "http://localhost:8080";

function wrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={qc}>{children}</QueryClientProvider>
  );
}

describe("usePollJob", () => {
  afterEach(() => server.resetHandlers());

  it("polls until SUCCESS, then reports isDone/isSuccess and stops polling", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/jobs/job-1`, () => {
        calls += 1;
        return HttpResponse.json({
          job: { id: "job-1", status: calls < 2 ? 2 : 3, query_result_id: calls < 2 ? null : 99, error: null },
        });
      }),
    );

    const { result } = renderHook(() => usePollJob("job-1", 10), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.isDone).toBe(true);
    expect(result.current.queryResultId).toBe(99);
    expect(result.current.isPollError).toBe(false);
    expect(result.current.gaveUp).toBe(false);

    const callsAtSuccess = calls;
    await new Promise((r) => setTimeout(r, 60));
    expect(calls).toBe(callsAtSuccess);
  });

  it("reports a real job FAILURE distinctly from a poll error", async () => {
    server.use(
      http.get(`${BASE}/api/jobs/job-2`, () =>
        HttpResponse.json({
          job: { id: "job-2", status: 4, query_result_id: null, error: "syntax error" },
        }),
      ),
    );

    const { result } = renderHook(() => usePollJob("job-2", 10), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.isFailure).toBe(true));
    expect(result.current.isDone).toBe(true);
    expect(result.current.isPollError).toBe(false);
    expect(result.current.error).toBe("syntax error");
  });

  it("a 404 (job TTL expired) settles into a poll error without retrying, and stops polling", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/jobs/job-3`, () => {
        calls += 1;
        return HttpResponse.json({ message: "not found" }, { status: 404 });
      }),
    );

    const { result } = renderHook(() => usePollJob("job-3", 10), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.isPollError).toBe(true));
    expect(result.current.isDone).toBe(true);
    expect(result.current.isFailure).toBe(true);
    expect(result.current.isPending).toBe(false);
    expect(result.current.isStarted).toBe(false);
    expect(result.current.error).toMatch(/lost track/i);
    expect(result.current.gaveUp).toBe(false);
    expect(calls).toBe(1);

    const callsAtError = calls;
    await new Promise((r) => setTimeout(r, 60));
    expect(calls).toBe(callsAtError);
  });

  it("a transient (500) error is retried a couple of times before settling into a poll error", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/jobs/job-4`, () => {
        calls += 1;
        return HttpResponse.json({ message: "boom" }, { status: 500 });
      }),
    );

    const { result } = renderHook(() => usePollJob("job-4", 10), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.isPollError).toBe(true), { timeout: 5000 });
    expect(calls).toBeGreaterThan(1);
  });

  it("returns an inert, disabled state when jobId is null", () => {
    const { result } = renderHook(() => usePollJob(null), { wrapper: wrapper() });
    expect(result.current.job).toBeNull();
    expect(result.current.isDone).toBe(false);
    expect(result.current.isPollError).toBe(false);
  });

  it("gives up after the attempt ceiling when a job never leaves PENDING, without ever calling it a failure", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/jobs/job-5`, () => {
        calls += 1;
        return HttpResponse.json({
          job: { id: "job-5", status: 1, query_result_id: null, error: null },
        });
      }),
    );

    const { result } = renderHook(() => usePollJob("job-5", 1), { wrapper: wrapper() });

    await waitFor(() => expect(result.current.gaveUp).toBe(true), { timeout: 5000 });
    expect(result.current.isDone).toBe(true);
    expect(result.current.isFailure).toBe(false);
    expect(result.current.isPollError).toBe(false);
    expect(result.current.isPending).toBe(false);
    expect(result.current.error).toMatch(/hasn't reported back/i);
    expect(calls).toBe(80);

    const callsAtGiveUp = calls;
    await new Promise((r) => setTimeout(r, 60));
    expect(calls).toBe(callsAtGiveUp);
  }, 8000);

  it("steps the poll interval up the ladder (fast, then slower, then slowest) instead of a fixed cadence", async () => {
    const timestamps: number[] = [];
    server.use(
      http.get(`${BASE}/api/jobs/job-6`, () => {
        timestamps.push(Date.now());
        return HttpResponse.json({
          job: { id: "job-6", status: 1, query_result_id: null, error: null },
        });
      }),
    );

    const { unmount } = renderHook(() => usePollJob("job-6", 20), { wrapper: wrapper() });

    await waitFor(() => expect(timestamps.length).toBeGreaterThanOrEqual(7));
    unmount();

    const gaps = timestamps.slice(1).map((t, i) => t - timestamps[i]);
    // Gaps 0-2 at the base interval, gaps 3-5 doubled: a coarse, timing-tolerant check on the
    // ladder shape.
    const earlyAvg = gaps.slice(0, 2).reduce((a, b) => a + b, 0) / 2;
    const laterAvg = gaps.slice(3, 5).reduce((a, b) => a + b, 0) / 2;
    expect(laterAvg).toBeGreaterThan(earlyAvg);
  });
});
