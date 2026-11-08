import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent, waitFor, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "./msw-server";
import SystemStatusPage from "@/pages/SystemStatusPage";

const BASE = "http://localhost:8080";

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <SystemStatusPage />
    </QueryClientProvider>,
  );
}

describe("SystemStatusPage", () => {
  it("colours job counts by what they mean, not by a blanket non-zero rule", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: {
            queued: 2,
            in_flight: 1,
            recent_success: 5,
            recent_failure: 0,
            recent_cancelled: 3,
          },
        }),
      ),
    );

    renderPage();

    const succeededCell = await screen.findByText("5");
    expect(succeededCell).toHaveClass("text-success");
    expect(succeededCell).not.toHaveClass("text-warning");

    const failedCell = screen.getByText("0");
    expect(failedCell).toHaveClass("text-muted-foreground");

    const queuedCell = screen.getByText("2");
    expect(queuedCell.className).not.toMatch(/text-(warning|destructive|success)/);
    const cancelledCell = screen.getByText("3");
    expect(cancelledCell.className).not.toMatch(/text-(warning|destructive|success)/);
  });

  it("colours a non-zero failure count red", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: {
            queued: 0,
            in_flight: 0,
            recent_success: 0,
            recent_failure: 4,
            recent_cancelled: 0,
          },
        }),
      ),
    );

    renderPage();

    const failedCell = await screen.findByText("4");
    expect(failedCell).toHaveClass("text-destructive");
  });

  it("shows a meaningful, sentence-case status word instead of the raw string", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 0 },
        }),
      ),
    );

    renderPage();

    expect(await screen.findByText("Operational")).toBeInTheDocument();
    expect(screen.queryByText("ok")).not.toBeInTheDocument();
  });

  it("shows Degraded (not the raw string) for a non-ok status", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "degraded",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 0 },
        }),
      ),
    );

    renderPage();

    expect(await screen.findByText("Degraded")).toBeInTheDocument();
  });

  it("shows the no-worker banner when no worker has checked in", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 0 },
        }),
      ),
    );

    renderPage();

    expect(await screen.findByText(/No worker has checked in/)).toBeInTheDocument();
  });

  it("shows an online worker count, never a per-worker hostname/id", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 3 },
        }),
      ),
    );

    renderPage();

    expect(await screen.findByText("3 workers online")).toBeInTheDocument();
    expect(screen.queryByText(/No worker has checked in/)).not.toBeInTheDocument();
  });

  it("shows a transient 'Refreshed' confirmation after a manual refresh, then clears it", async () => {
    server.use(
      http.get(`${BASE}/api/status`, () =>
        HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 0 },
        }),
      ),
    );

    renderPage();
    expect(await screen.findByText("Operational")).toBeInTheDocument();
    expect(screen.queryByText("Refreshed")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Refresh/ }));

    expect(await screen.findByText("Refreshed")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText("Refreshed")).not.toBeInTheDocument(), { timeout: 3500 });
  });

  it("does not show 'Refreshed' when a manual refresh fails", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/status`, () => {
        calls += 1;
        if (calls === 1) {
          return HttpResponse.json({
            status: "ok",
            version: "1.0.0",
            jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
            workers: { online: 0 },
          });
        }
        return HttpResponse.json({ message: "boom" }, { status: 500 });
      }),
    );

    renderPage();
    expect(await screen.findByText("Operational")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /Refresh/ }));

    await waitFor(() => expect(screen.getByText("Failed to load status.")).toBeInTheDocument());
    expect(screen.queryByText("Refreshed")).not.toBeInTheDocument();
  });

  it("the 'Updated N ago' label advances without a refetch", async () => {
    let calls = 0;
    server.use(
      http.get(`${BASE}/api/status`, async () => {
        calls += 1;
        // Later calls hang, standing in for a poll in flight; dataUpdatedAt must not move.
        if (calls > 1) await new Promise(() => {});
        return HttpResponse.json({
          status: "ok",
          version: "1.0.0",
          jobs: { queued: 0, in_flight: 0, recent_success: 0, recent_failure: 0, recent_cancelled: 0 },
          workers: { online: 1 },
        });
      }),
    );

    // Fake timers from before mount so useNow's setInterval is controlled.
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      renderPage();
      expect(await screen.findByText(/^Updated just now$/)).toBeInTheDocument();

      // timeAgo leaves "just now" past 45s and useNow ticks every 30s, so the 60s tick is the first
      // that can change the label.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(60_100);
      });

      expect(screen.getByText(/^Updated 1 min ago$/)).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });
});
