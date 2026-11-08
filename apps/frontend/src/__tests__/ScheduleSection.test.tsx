/** `@/lib/api` is mocked wholesale (not MSW) so the exact body each mutation sends is asserted
 * directly. */
import type { ComponentProps } from "react";
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { FreshnessChip } from "@/components/scheduling/FreshnessChip";
import { ScheduleSection, QUERY_FLAVOR, MONITOR_FLAVOR } from "@/components/scheduling/ScheduleSection";
import { RunsTable } from "@/components/scheduling/RunsTable";
import type { Run, Schedule } from "@/types/api";

vi.mock("@/lib/api", () => {
  class ApiError extends Error {
    status: number;
    constructor(status: number, message: string) {
      super(message);
      this.name = "ApiError";
      this.status = status;
    }
  }
  return {
    ApiError,
    api: {
      getSchedule: vi.fn(),
      listRuns: vi.fn(),
      getQueryConsumers: vi.fn(),
      createSchedule: vi.fn(),
      updateSchedule: vi.fn(),
      deleteSchedule: vi.fn(),
      runNow: vi.fn(),
      runResultCsvUrl: (id: number) => `http://localhost:8080/api/runs/${id}/result.csv`,
    },
  };
});

import { ApiError, api } from "@/lib/api";

const mockApi = api as unknown as {
  getSchedule: ReturnType<typeof vi.fn>;
  listRuns: ReturnType<typeof vi.fn>;
  getQueryConsumers: ReturnType<typeof vi.fn>;
  createSchedule: ReturnType<typeof vi.fn>;
  updateSchedule: ReturnType<typeof vi.fn>;
  deleteSchedule: ReturnType<typeof vi.fn>;
  runNow: ReturnType<typeof vi.fn>;
};

function makeSchedule(overrides: Partial<Schedule> = {}): Schedule {
  return {
    id: 1,
    entity_type: "query",
    entity_id: 1,
    spec: { kind: "interval", every_secs: 900 },
    timezone: "UTC",
    enabled: true,
    paused_until: null,
    auto_paused_at: null,
    overlap_policy: "skip",
    next_run_at: new Date(Date.now() + 6 * 60_000).toISOString(),
    consecutive_failures: 0,
    last_run: {
      status: "success",
      finished_at: new Date(Date.now() - 8 * 60_000).toISOString(),
      duration_ms: 3200,
      error: null,
    },
    ...overrides,
  };
}

function makeRun(overrides: Partial<Run> = {}): Run {
  return {
    id: 1,
    status: "success",
    triggered_by: "schedule",
    scheduled_for: null,
    started_at: new Date(Date.now() - 8 * 60_000).toISOString(),
    finished_at: new Date(Date.now() - 8 * 60_000 + 3200).toISOString(),
    duration_ms: 3200,
    row_count: 1204,
    error: null,
    data_age_secs: null,
    waiting_on_run_id: null,
    query_result_id: 55,
    condition_met: null,
    notified: [],
    ...overrides,
  };
}

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

beforeEach(() => {
  vi.clearAllMocks();
  mockApi.getSchedule.mockResolvedValue(null);
  mockApi.listRuns.mockResolvedValue([]);
  mockApi.getQueryConsumers.mockResolvedValue({ tile_count: 0, dashboard_count: 0, monitor_count: 0 });
});

describe("FreshnessChip", () => {
  function renderChip(onOpenFailed = vi.fn()) {
    const qc = makeClient();
    return render(
      <QueryClientProvider client={qc}>
        <FreshnessChip queryId={1} lifecycle="published_clean" onOpenFailed={onOpenFailed} />
      </QueryClientProvider>,
    );
  }

  it("renders nothing when there is no schedule", async () => {
    mockApi.getSchedule.mockResolvedValue(null);
    const { container } = renderChip();
    await waitFor(() => expect(mockApi.getSchedule).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it("renders nothing when the schedule exists but has never run and none is in flight", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule({ last_run: null }));
    const { container } = renderChip();
    await waitFor(() => expect(mockApi.getSchedule).toHaveBeenCalled());
    expect(container).toBeEmptyDOMElement();
  });

  it("shows 'Updated 8 min ago' neutral for a fresh success", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    renderChip();
    expect(await screen.findByText(/Updated 8 min ago/)).toBeInTheDocument();
  });

  it("shows amber 'Updated 2h ago' when past 1.5x the interval", async () => {
    mockApi.getSchedule.mockResolvedValue(
      makeSchedule({
        last_run: {
          status: "success",
          finished_at: new Date(Date.now() - 2 * 3600_000).toISOString(),
          duration_ms: 1000,
          error: null,
        },
      }),
    );
    renderChip();
    const chip = await screen.findByText(/Updated 2h ago/);
    expect(chip.closest("span")).toHaveClass("bg-warning/10");
  });

  it("shows red 'Last refresh failed …' with an error affordance and calls onOpenFailed on click", async () => {
    mockApi.getSchedule.mockResolvedValue(
      makeSchedule({
        last_run: {
          status: "failed",
          finished_at: new Date(Date.now() - 20 * 60_000).toISOString(),
          duration_ms: 400,
          error: "connector error",
        },
      }),
    );
    mockApi.listRuns.mockResolvedValue([makeRun({ id: 77, status: "failed" })]);
    const onOpenFailed = vi.fn();
    renderChip(onOpenFailed);

    const chipButton = await screen.findByText(/Last refresh failed/);
    await userEvent.click(chipButton);
    expect(onOpenFailed).toHaveBeenCalledWith(77);
  });

  it("shows 'Refreshing… (elapsed)' with a spinner for an in-flight run", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([
      makeRun({ id: 2, status: "running", started_at: new Date(Date.now() - 12_000).toISOString() }),
    ]);
    renderChip();
    const chip = await screen.findByText(/Refreshing…/);
    expect(chip.closest('[data-testid="freshness-chip"]')?.querySelector("svg")).toBeInTheDocument();
  });

  it("titles the chip 'Published data — updated …' only when lifecycle is published_pending", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    const qc = makeClient();
    render(
      <QueryClientProvider client={qc}>
        <FreshnessChip queryId={1} lifecycle="published_pending" onOpenFailed={vi.fn()} />
      </QueryClientProvider>,
    );
    const chip = await screen.findByText(/Updated 8 min ago/);
    expect(chip).toHaveAttribute("title", expect.stringContaining("Published data — updated"));
  });
});

describe("ScheduleSection", () => {
  function renderSection(overrides: Partial<ComponentProps<typeof ScheduleSection>> = {}) {
    const qc = makeClient();
    const utils = render(
      <QueryClientProvider client={qc}>
        <ScheduleSection
          entityId={1}
          flavor={QUERY_FLAVOR}
          canEdit={true}
          published={true}
          lifecycle="published_clean"
          onPublishClick={vi.fn()}
          expanded={true}
          onExpandedChange={vi.fn()}
          focusRunId={null}
          {...overrides}
        />
      </QueryClientProvider>,
    );
    return utils;
  }

  it("never-published: renders a disabled toggle, the precondition copy, and a Publish button when canEdit", async () => {
    renderSection({ published: false, canEdit: true });
    expect(await screen.findByText(/Publish this query to schedule refreshes/)).toBeInTheDocument();
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Publish…" })).toBeInTheDocument();
  });

  it("never-published: omits the Publish button when canEdit is false", async () => {
    renderSection({ published: false, canEdit: false });
    expect(await screen.findByText(/Publish this query to schedule refreshes/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
  });

  it("published_clean: renders no published-version sub-note", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    renderSection({ lifecycle: "published_clean" });
    await screen.findByRole("switch");
    expect(
      screen.queryByText("Refreshes run the published version, not your unpublished edits."),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Runs the published version.")).not.toBeInTheDocument();
  });

  it("published_pending: shows the published-version sub-notes", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    renderSection({ lifecycle: "published_pending" });
    expect(
      await screen.findByText("Refreshes run the published version, not your unpublished edits."),
    ).toBeInTheDocument();
    expect(screen.getByText("Runs the published version.")).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: /learn more about scheduled refresh/i }),
    ).toHaveAttribute("href", "https://www.arcodash.com/docs/queries-visualization/scheduled-refresh");
  });

  it("Off→On with no schedule row calls createSchedule with the default Every-15-min spec", async () => {
    mockApi.getSchedule.mockResolvedValue(null);
    mockApi.createSchedule.mockResolvedValue(makeSchedule());
    renderSection();

    const toggle = await screen.findByRole("switch");
    await userEvent.click(toggle);

    await waitFor(() =>
      expect(mockApi.createSchedule).toHaveBeenCalledWith({
        entity_type: "query",
        entity_id: 1,
        spec: { kind: "interval", every_secs: 900 },
      }),
    );
  });

  it("On→Off calls updateSchedule with { enabled: false } (does not delete)", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule({ enabled: true }));
    mockApi.updateSchedule.mockResolvedValue(makeSchedule({ enabled: false }));
    renderSection();

    const toggle = await screen.findByRole("switch");
    await waitFor(() => expect(toggle).toBeChecked());
    await userEvent.click(toggle);

    await waitFor(() => expect(mockApi.updateSchedule).toHaveBeenCalledWith(1, { enabled: false }));
    expect(mockApi.deleteSchedule).not.toHaveBeenCalled();
  });

  it("Off→On with an existing (disabled) schedule row calls updateSchedule, not createSchedule", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule({ enabled: false }));
    mockApi.updateSchedule.mockResolvedValue(makeSchedule({ enabled: true }));
    renderSection();

    const toggle = await screen.findByRole("switch");
    await waitFor(() => expect(toggle).not.toBeChecked());
    await userEvent.click(toggle);

    await waitFor(() => expect(mockApi.updateSchedule).toHaveBeenCalledWith(1, { enabled: true }));
    expect(mockApi.createSchedule).not.toHaveBeenCalled();
  });

  it("changing the cadence preset calls updateSchedule with the mapped spec", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.updateSchedule.mockResolvedValue(makeSchedule());
    renderSection();

    await screen.findByRole("switch");
    const presetSelect = screen.getByRole("combobox", { name: "Refresh every:" });
    await userEvent.click(presetSelect);
    await userEvent.click(await screen.findByRole("option", { name: "Hourly" }));

    await waitFor(() =>
      expect(mockApi.updateSchedule).toHaveBeenCalledWith(1, {
        spec: { kind: "interval", every_secs: 3600 },
      }),
    );
  });

  it("Advanced disclosure stays collapsed on open even when the spec is a raw cron", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule({ spec: { kind: "cron", expr: "0 30 9 * * MON" } }));
    renderSection();
    expect(await screen.findByText(/Custom cadence: Cron: 0 30 9 \* \* MON/)).toBeInTheDocument();
    expect(screen.queryByPlaceholderText("0 */15 * * * *")).not.toBeInTheDocument();
  });

  it("the cadence dropdown offers no mode that submits nothing", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.updateSchedule.mockResolvedValue(makeSchedule());
    renderSection();

    await screen.findByRole("switch");
    const presetSelect = screen.getByRole("combobox", { name: "Refresh every:" });
    await userEvent.click(presetSelect);
    const optionLabels = within(await screen.findByRole("listbox"))
      .getAllByRole("option")
      .map((o) => o.textContent);
    // Cron has its own entry point (Advanced disclosure); it must not also appear in this list.
    expect(optionLabels).not.toContain("Advanced (cron)");
    await userEvent.keyboard("{Escape}");

    for (const label of optionLabels as string[]) {
      mockApi.updateSchedule.mockClear();
      await userEvent.click(presetSelect);
      await userEvent.click(await screen.findByRole("option", { name: label }));
      await waitFor(() => expect(mockApi.updateSchedule).toHaveBeenCalled());
    }
  });

  it("a bad cron surfaces the backend 400 inline, not a toast", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule({ spec: { kind: "cron", expr: "bad" } }));
    mockApi.updateSchedule.mockRejectedValue(new ApiError(400, "invalid cron expression"));
    renderSection();

    await userEvent.click(await screen.findByText("▸ Advanced (cron)"));
    const cronInput = screen.getByPlaceholderText("0 */15 * * * *");
    await userEvent.clear(cronInput);
    await userEvent.type(cronInput, "still bad");
    await userEvent.tab();

    expect(await screen.findByText("invalid cron expression")).toBeInTheDocument();
  });

  it("renders a success run row with a CSV link and a failed row with an error expander", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([
      makeRun({ id: 10, status: "success", query_result_id: 99 }),
      makeRun({ id: 11, status: "failed", query_result_id: null, error: "connection refused" }),
    ]);
    renderSection();

    expect(await screen.findByRole("link", { name: "⬇ CSV" })).toHaveAttribute(
      "href",
      "http://localhost:8080/api/runs/10/result.csv",
    );
    expect(screen.getByRole("button", { name: "Show error" })).toBeInTheDocument();
  });

  it("clicking the error affordance opens the verbatim error in a <pre> with a Retry button; a second click collapses it", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([makeRun({ id: 12, status: "failed", error: "boom: table missing" })]);
    renderSection();

    const errorToggle = await screen.findByRole("button", { name: "Show error" });
    await userEvent.click(errorToggle);
    expect(screen.getByText("boom: table missing")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();

    await userEvent.click(errorToggle);
    expect(screen.queryByText("boom: table missing")).not.toBeInTheDocument();
  });

  it("renders the empty-runs row and hides the footer when there are no runs", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([]);
    renderSection();

    expect(await screen.findByText("No runs yet — the first refresh will appear here.")).toBeInTheDocument();
    expect(screen.queryByText(/Showing the last 10 runs/)).not.toBeInTheDocument();
  });

  it("renders the last-10 footer once there is ≥1 run", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([makeRun()]);
    renderSection();

    expect(
      await screen.findByText("Showing the last 10 runs (older runs aren't retained)."),
    ).toBeInTheDocument();
  });

  it("shows the amber auto-pause banner when auto_paused_at is set; Resume calls updateSchedule({ enabled: true })", async () => {
    mockApi.getSchedule.mockResolvedValue(
      makeSchedule({ enabled: false, auto_paused_at: new Date().toISOString(), consecutive_failures: 10 }),
    );
    mockApi.updateSchedule.mockResolvedValue(makeSchedule({ enabled: true }));
    renderSection();

    expect(
      await screen.findByText(/Refreshes paused after 10 consecutive failures\./),
    ).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Resume" }));
    await waitFor(() => expect(mockApi.updateSchedule).toHaveBeenCalledWith(1, { enabled: true }));
  });

  it("renders the collapsed '· Paused' marker when paused", async () => {
    mockApi.getSchedule.mockResolvedValue(
      makeSchedule({ enabled: false, auto_paused_at: new Date().toISOString() }),
    );
    renderSection({ expanded: false });
    expect(await screen.findByText("· Paused")).toBeInTheDocument();
  });

  it("impact line reads 'Feeds N tiles on M dashboards · K monitors', drops a zero segment, and hides entirely when all counts are zero", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.getQueryConsumers.mockResolvedValue({ tile_count: 4, dashboard_count: 2, monitor_count: 0 });
    const { rerender } = renderSection();
    expect(await screen.findByText("Feeds 4 tiles on 2 dashboards")).toBeInTheDocument();

    mockApi.getQueryConsumers.mockResolvedValue({ tile_count: 0, dashboard_count: 0, monitor_count: 0 });
    const qc = makeClient();
    rerender(
      <QueryClientProvider client={qc}>
        <ScheduleSection
          entityId={2}
          flavor={QUERY_FLAVOR}
          canEdit={true}
          published={true}
          lifecycle="published_clean"
          onPublishClick={vi.fn()}
          expanded={true}
          onExpandedChange={vi.fn()}
          focusRunId={null}
        />
      </QueryClientProvider>,
    );
    await waitFor(() => expect(mockApi.getQueryConsumers).toHaveBeenCalledWith(2));
    expect(screen.queryByText(/^Feeds/)).not.toBeInTheDocument();
  });

  it("canEdit=false disables toggle/cadence/Run now/Remove but keeps the runs table + CSV interactive", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.listRuns.mockResolvedValue([makeRun({ id: 20, query_result_id: 5 })]);
    renderSection({ canEdit: false });

    expect(await screen.findByRole("switch")).toBeDisabled();
    for (const combobox of screen.getAllByRole("combobox")) {
      expect(combobox).toBeDisabled();
    }
    expect(screen.getByRole("button", { name: "Run now" })).toBeDisabled();
    expect(await screen.findByRole("button", { name: "Remove schedule" })).toBeDisabled();
    expect(await screen.findByRole("link", { name: "⬇ CSV" })).not.toHaveAttribute("aria-disabled");
  });

  it("canEdit=true: Remove schedule is enabled and clicking it calls deleteSchedule", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.deleteSchedule.mockResolvedValue(undefined);
    renderSection({ canEdit: true });

    const removeButton = await screen.findByRole("button", { name: "Remove schedule" });
    expect(removeButton).not.toBeDisabled();
    await userEvent.click(removeButton);

    await waitFor(() => expect(mockApi.deleteSchedule).toHaveBeenCalledWith(1));
  });

  it("a background poll returning a new (but equal) spec object doesn't reset an in-progress cron edit", async () => {
    const schedule = makeSchedule({ spec: { kind: "cron", expr: "0 30 9 * * MON" } });
    mockApi.getSchedule.mockResolvedValue(schedule);
    const qc = makeClient();
    render(
      <QueryClientProvider client={qc}>
        <ScheduleSection
          entityId={1}
          flavor={QUERY_FLAVOR}
          canEdit={true}
          published={true}
          lifecycle="published_clean"
          onPublishClick={vi.fn()}
          expanded={true}
          onExpandedChange={vi.fn()}
          focusRunId={null}
        />
      </QueryClientProvider>,
    );

    await userEvent.click(await screen.findByText("▸ Advanced (cron)"));
    const cronInput = screen.getByPlaceholderText("0 */15 * * * *");
    await userEvent.clear(cronInput);
    await userEvent.type(cronInput, "0 0 * * * *");
    expect(cronInput).toHaveValue("0 0 * * * *");

    // Same id and value but a fresh object, as the poll's refetchInterval delivers every tick.
    mockApi.getSchedule.mockResolvedValue({ ...schedule, spec: { ...schedule.spec } });
    await act(async () => {
      await qc.refetchQueries({ queryKey: ["schedules", "query", 1] });
    });

    expect(cronInput).toHaveValue("0 0 * * * *");
  });

  it("shows an inline message when removing the schedule fails", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.deleteSchedule.mockRejectedValue(new ApiError(500, "could not remove schedule"));
    renderSection({ canEdit: true });

    await userEvent.click(await screen.findByRole("button", { name: "Remove schedule" }));
    expect(await screen.findByText("could not remove schedule")).toBeInTheDocument();
  });

  it("shows an inline message when Run now fails", async () => {
    mockApi.getSchedule.mockResolvedValue(makeSchedule());
    mockApi.runNow.mockRejectedValue(new ApiError(500, "could not run now"));
    renderSection();

    await userEvent.click(await screen.findByRole("button", { name: "Run now" }));
    expect(await screen.findByText("could not run now")).toBeInTheDocument();
  });
});

describe("ScheduleSection — monitor flavor", () => {
  const QUERY_SUMMARY = { id: 7, name: "Signups query" };

  function renderMonitorSection(overrides: Partial<ComponentProps<typeof ScheduleSection>> = {}) {
    const qc = makeClient();
    return render(
      <QueryClientProvider client={qc}>
        <MemoryRouter>
          <ScheduleSection
            entityId={42}
            flavor={MONITOR_FLAVOR}
            canEdit={true}
            expanded={true}
            onExpandedChange={vi.fn()}
            focusRunId={null}
            readsQuery={QUERY_SUMMARY}
            monitorControls={
              <div>
                <label>Notify — how often you're told while this stays alerting</label>
                <label>When a refresh is already running</label>
              </div>
            }
            {...overrides}
          />
        </MemoryRouter>
      </QueryClientProvider>,
    );
  }

  /** Routes `getSchedule` by entity type: the monitor's own schedule vs `ReadsQueryLine`'s fetch of
   * the query's schedule. */
  function mockSchedules(monitorSchedule: Schedule | null, querySchedule: Schedule | null) {
    mockApi.getSchedule.mockImplementation((entityType: string) =>
      Promise.resolve(entityType === "monitor" ? monitorSchedule : querySchedule),
    );
  }

  it("renders the Check-schedule title, Eye/amber accent, and 'Check every:' cadence label", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    renderMonitorSection();

    expect(await screen.findByText("Check schedule")).toBeInTheDocument();
    const icon = document.querySelector("svg.text-warning");
    expect(icon).toBeInTheDocument();
    expect(screen.getByText("Check every:")).toBeInTheDocument();
  });

  it("renders the 'Reads <query> · refreshed …' line linking to /queries/:id", async () => {
    mockSchedules(
      makeSchedule({ entity_type: "monitor", entity_id: 42 }),
      makeSchedule({
        entity_type: "query",
        entity_id: 7,
        last_run: {
          status: "success",
          finished_at: new Date(Date.now() - 8 * 60_000).toISOString(),
          duration_ms: 1000,
          error: null,
        },
      }),
    );
    renderMonitorSection();

    const link = await screen.findByRole("link", { name: "Signups query" });
    expect(link).toHaveAttribute("href", "/queries/7");
    expect(await screen.findByText(/refreshed 8 min ago/)).toBeInTheDocument();
  });

  // Copy describes the schedule, distinguishing "no schedule" from "scheduled but not fired yet".
  it("amber 'no refresh schedule set' when the underlying query has no schedule", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    renderMonitorSection();

    const text = await screen.findByText(/no refresh schedule set/);
    expect(text.closest("p")).toHaveClass("text-warning-foreground");
  });

  it("amber 'scheduled, not run yet' when the query has a schedule but no last_run", async () => {
    mockSchedules(
      makeSchedule({ entity_type: "monitor", entity_id: 42 }),
      makeSchedule({ entity_type: "query", entity_id: 7, last_run: null }),
    );
    renderMonitorSection();

    const text = await screen.findByText(/scheduled, not run yet/);
    expect(text.closest("p")).toHaveClass("text-warning-foreground");
  });

  it("renders the relocated Notify control and the Max-wait control from monitorControls", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    renderMonitorSection();

    expect(
      await screen.findByText("Notify — how often you're told while this stays alerting"),
    ).toBeInTheDocument();
    expect(screen.getByText("When a refresh is already running")).toBeInTheDocument();
  });

  it("shows the Off≠Mute note when the schedule is disabled", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42, enabled: false }), null);
    renderMonitorSection();

    expect(
      await screen.findByText(/Checking is off — this condition is not being evaluated at all/),
    ).toBeInTheDocument();
  });

  it("never renders the never-published inert branch for a monitor", async () => {
    mockSchedules(null, null);
    renderMonitorSection();

    await screen.findByText("Check schedule");
    expect(
      screen.queryByText(/Publish this query to schedule refreshes/),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("switch")).not.toBeDisabled();
  });

  it("does not fetch query consumers or render an impact line for a monitor", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    mockApi.getQueryConsumers.mockResolvedValue({ tile_count: 4, dashboard_count: 1, monitor_count: 2 });
    renderMonitorSection();

    await screen.findByText("Check schedule");
    expect(mockApi.getQueryConsumers).not.toHaveBeenCalled();
    expect(screen.queryByText(/^Feeds/)).not.toBeInTheDocument();
  });

  it("shows the neutral 'Waiting for fresh data…' badge when the in-flight run has waiting_on_run_id", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    mockApi.listRuns.mockResolvedValue([
      makeRun({
        id: 30,
        status: "running",
        started_at: new Date(Date.now() - 12_000).toISOString(),
        waiting_on_run_id: 999,
      }),
    ]);
    renderMonitorSection();

    expect(await screen.findByText("Waiting for fresh data…")).toBeInTheDocument();
  });

  it("Off→On with no schedule calls createSchedule with entity_type: monitor", async () => {
    mockSchedules(null, null);
    mockApi.createSchedule.mockResolvedValue(makeSchedule({ entity_type: "monitor", entity_id: 42 }));
    renderMonitorSection();

    const toggle = await screen.findByRole("switch");
    await userEvent.click(toggle);

    await waitFor(() =>
      expect(mockApi.createSchedule).toHaveBeenCalledWith({
        entity_type: "monitor",
        entity_id: 42,
        spec: { kind: "interval", every_secs: 900 },
      }),
    );
  });

  it("Check now calls runNow('monitor', id)", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    mockApi.runNow.mockResolvedValue(makeRun({ id: 31 }));
    renderMonitorSection();

    await userEvent.click(await screen.findByRole("button", { name: "Check now" }));
    await waitFor(() => expect(mockApi.runNow).toHaveBeenCalledWith("monitor", 42));
  });

  describe("after Check now", () => {
    const pending = [{ destination_name: "Slack", status: "pending", error: null }];
    const sent = [{ destination_name: "Slack", status: "sent", error: null }];

    async function clickCheckNow() {
      mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
      mockApi.runNow.mockResolvedValue(makeRun({ id: 31 }));
      renderMonitorSection();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(10);
      });
      await act(async () => {
        screen.getByRole("button", { name: "Check now" }).click();
        await vi.advanceTimersByTimeAsync(10);
      });
    }

    const tick = (ms: number) =>
      act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });

    beforeEach(() => vi.useFakeTimers());
    afterEach(() => vi.useRealTimers());

    it("keeps refetching a terminal run until its notification settles, then stops", async () => {
      mockApi.listRuns
        .mockResolvedValueOnce([])
        .mockResolvedValueOnce([makeRun({ id: 31, notified: pending })])
        .mockResolvedValueOnce([makeRun({ id: 31, notified: pending })])
        .mockResolvedValue([makeRun({ id: 31, notified: sent })]);
      await clickCheckNow();
      await tick(10_000);
      expect(screen.getByText("Sent to Slack")).toBeInTheDocument();
      const settledCalls = mockApi.listRuns.mock.calls.length;
      expect(settledCalls).toBeGreaterThanOrEqual(4);
      await tick(20_000);
      expect(mockApi.listRuns.mock.calls.length).toBe(settledCalls);
    });

    const detailInvalidations = (spy: { mock: { calls: unknown[][] } }, id: number | string) =>
      spy.mock.calls.filter(
        ([f]) => JSON.stringify((f as { queryKey?: unknown }).queryKey) === JSON.stringify(["monitor", id]),
      ).length;

    it("refreshes the monitor detail query (both key shapes) only after the run settles", async () => {
      const spy = vi.spyOn(QueryClient.prototype, "invalidateQueries");
      mockApi.listRuns
        .mockResolvedValueOnce([])
        .mockResolvedValueOnce([makeRun({ id: 31, notified: pending })])
        .mockResolvedValueOnce([makeRun({ id: 31, notified: pending })])
        .mockResolvedValue([makeRun({ id: 31, notified: sent })]);
      await clickCheckNow();
      await tick(1_600);
      expect(screen.queryByText("Sent to Slack")).not.toBeInTheDocument();
      expect(detailInvalidations(spy, 42)).toBe(0);
      expect(detailInvalidations(spy, "42")).toBe(0);
      await tick(10_000);
      expect(screen.getByText("Sent to Slack")).toBeInTheDocument();
      expect(detailInvalidations(spy, 42)).toBe(1);
      expect(detailInvalidations(spy, "42")).toBe(1);
      spy.mockRestore();
    });

    it("refreshes the monitor detail query once when the window lapses unsettled", async () => {
      const spy = vi.spyOn(QueryClient.prototype, "invalidateQueries");
      mockApi.listRuns
        .mockResolvedValueOnce([])
        .mockResolvedValue([makeRun({ id: 31, notified: pending })]);
      await clickCheckNow();
      await tick(20_000);
      expect(detailInvalidations(spy, 42)).toBe(0);
      await tick(15_000);
      expect(detailInvalidations(spy, 42)).toBe(1);
      expect(detailInvalidations(spy, "42")).toBe(1);
      await tick(30_000);
      expect(detailInvalidations(spy, 42)).toBe(1);
      spy.mockRestore();
    });

    it("gives up polling once the window elapses", async () => {
      mockApi.listRuns
        .mockResolvedValueOnce([])
        .mockResolvedValue([makeRun({ id: 31, notified: pending })]);
      await clickCheckNow();
      await tick(35_000);
      const capped = mockApi.listRuns.mock.calls.length;
      expect(capped).toBeGreaterThan(5);
      await tick(30_000);
      expect(mockApi.listRuns.mock.calls.length).toBe(capped);
    });
  });

  it("canEdit=false disables toggle/cadence/Check now but keeps the runs table interactive", async () => {
    mockSchedules(makeSchedule({ entity_type: "monitor", entity_id: 42 }), null);
    mockApi.listRuns.mockResolvedValue([makeRun({ id: 32, query_result_id: 5 })]);
    renderMonitorSection({ canEdit: false });

    expect(await screen.findByRole("switch")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Check now" })).toBeDisabled();
    expect(await screen.findByRole("link", { name: "⬇ CSV" })).not.toHaveAttribute("aria-disabled");
  });
});

describe("RunsTable — monitor flavor", () => {
  function renderTable(runs: Run[], overrides: Partial<ComponentProps<typeof RunsTable>> = {}) {
    return render(
      <RunsTable
        entityType="monitor"
        runs={runs}
        now={Date.now()}
        openErrorRunId={null}
        onToggleError={vi.fn()}
        onRetry={vi.fn()}
        canRetry={true}
        {...overrides}
      />,
    );
  }

  it("Check column: Ran when success; Failed to run+ⓘ when failed — never says Alerting on a check row", () => {
    renderTable([
      makeRun({ id: 1, status: "success", condition_met: false }),
      makeRun({ id: 2, status: "success", condition_met: true }),
      makeRun({ id: 3, status: "failed", condition_met: null, error: "boom" }),
      makeRun({ id: 4, status: "success", condition_met: null }),
    ]);

    expect(screen.getAllByText("Ran")).toHaveLength(3);
    expect(screen.getByText("Failed to run").closest("span")).toHaveClass("text-destructive");
    expect(screen.queryByText("Alerting")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show error" })).toBeInTheDocument();
  });

  it("Result column: OK-styled 'Condition not met', warning-styled 'Condition met', never sharing color/badge with Failed to run", () => {
    renderTable([
      makeRun({ id: 1, status: "success", condition_met: true }),
      makeRun({ id: 2, status: "failed", error: "boom" }),
    ]);

    const met = screen.getByText("Condition met");
    const failed = screen.getByText("Failed to run");
    expect(met.closest("span")).toHaveClass("text-warning-foreground");
    expect(failed.closest("span")).toHaveClass("text-destructive");
    expect(met.textContent).not.toBe(failed.textContent);
    expect(met.closest("span")?.className).not.toBe(failed.closest("span")?.className);
  });

  it("Result column reads 'Condition met'/'Condition not met'/'—'", () => {
    renderTable([
      makeRun({ id: 1, condition_met: true }),
      makeRun({ id: 2, condition_met: false }),
      makeRun({ id: 3, condition_met: null }),
    ]);

    expect(screen.getByText("Condition met")).toBeInTheDocument();
    expect(screen.getByText("Condition not met")).toBeInTheDocument();
    // Data-age also renders "—"; scope to the Result column (4th td).
    const row3 = screen.getAllByText("Ran")[2].closest("tr");
    expect(within(row3!).getAllByRole("cell")[3]).toHaveTextContent("—");
  });

  it("Data-age cell turns amber with the ceiling hover when data_age_secs exceeds maxFreshWaitSecs", () => {
    renderTable([makeRun({ id: 1, data_age_secs: 700 })], { maxFreshWaitSecs: 600 });

    const cell = screen.getByText("data 11m 40s old");
    expect(cell).toHaveClass("text-warning-foreground");
    expect(cell).toHaveAttribute(
      "title",
      expect.stringContaining("we'll re-check as soon as fresh data is ready"),
    );
  });

  it("Data-age cell stays neutral when maxFreshWaitSecs is null (always wait)", () => {
    renderTable([makeRun({ id: 1, data_age_secs: 1_000_000 })], { maxFreshWaitSecs: null });

    const cell = screen.getByText(/^data .+ old$/);
    expect(cell).not.toHaveClass("text-warning-foreground");
  });

  it("Notification: 'Not sent — state unchanged' for [], 'Sent to <dest>' for sent, red 'Failed: <dest>' when any failed", () => {
    renderTable([
      makeRun({ id: 1, notified: [] }),
      makeRun({ id: 2, notified: [{ destination_name: "#alerts", status: "sent", error: null }] }),
      makeRun({
        id: 3,
        notified: [
          { destination_name: "#alerts", status: "sent", error: null },
          { destination_name: "webhook", status: "failed", error: "boom" },
        ],
      }),
    ]);

    expect(screen.getByText("Not sent — state unchanged")).toBeInTheDocument();
    expect(screen.getAllByText(/Sent to #alerts/).length).toBeGreaterThan(0);
    const failed = screen.getByText("Failed: webhook");
    expect(failed).toHaveClass("text-destructive");
  });

  it("renders the monitor empty-state and 'last 10 checks' footer with the first-row disclosure", () => {
    const empty = renderTable([]);
    expect(
      screen.getByText("No checks yet — the first check will appear here."),
    ).toBeInTheDocument();
    empty.unmount();

    renderTable([makeRun({ id: 1 })]);
    expect(
      screen.getByText(
        "Showing the last 10 checks (older checks aren't retained). Each check reads the first row of the result.",
      ),
    ).toBeInTheDocument();
  });
});
