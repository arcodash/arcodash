import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RunsTable } from "./RunsTable";
import type { Run } from "@/types/api";

function baseRun(overrides: Partial<Run> = {}): Run {
  return {
    id: 1,
    status: "success",
    triggered_by: "schedule",
    scheduled_for: null,
    started_at: "2026-08-05T00:00:00Z",
    finished_at: "2026-08-05T00:00:05Z",
    duration_ms: 5000,
    row_count: null,
    error: null,
    data_age_secs: null,
    waiting_on_run_id: null,
    query_result_id: null,
    condition_met: null,
    notified: [],
    ...overrides,
  };
}

function renderTable(runs: Run[], overrides: Partial<React.ComponentProps<typeof RunsTable>> = {}) {
  return render(
    <RunsTable
      entityType="monitor"
      runs={runs}
      now={Date.parse("2026-08-05T00:10:00Z")}
      openErrorRunId={null}
      onToggleError={vi.fn()}
      onRetry={vi.fn()}
      canRetry
      {...overrides}
    />,
  );
}

describe("RunsTable — monitor check/result columns", () => {
  it("renders a distinct Couldn't evaluate result for a success run with no condition_met but an error reason", () => {
    renderTable([
      baseRun({ condition_met: null, error: "Column \"total\" is not present in the query result." }),
    ]);
    expect(screen.getByText("Ran")).toBeInTheDocument();
    expect(screen.getByText("Couldn't evaluate")).toBeInTheDocument();
    // Must not be mistaken for the quiet "no result yet" state or a hard failure.
    expect(screen.queryByText("Failed to run")).not.toBeInTheDocument();
  });

  it("renders Ran + — for a success run with no condition_met and no error", () => {
    renderTable([baseRun({ condition_met: null, error: null })]);
    expect(screen.getByText("Ran")).toBeInTheDocument();
    // "—" also appears in the Data-age column for this row (data_age_secs is null) — both are
    // legitimately blank, so assert the count rather than a single ambiguous match.
    expect(screen.getAllByText("—")).toHaveLength(2);
    expect(screen.queryByText("Couldn't evaluate")).not.toBeInTheDocument();
  });

  it("splits condition-met/not-met into the Result column, never showing Alerting on a check row", () => {
    renderTable([baseRun({ id: 1, condition_met: true }), baseRun({ id: 2, condition_met: false })]);
    expect(screen.getByText("Condition met")).toBeInTheDocument();
    expect(screen.getByText("Condition not met")).toBeInTheDocument();
    expect(screen.queryByText("Alerting")).not.toBeInTheDocument();
  });

  it("renders unambiguous copy for a healthy row: Ran · Condition not met · Not sent — state unchanged", () => {
    renderTable([baseRun({ condition_met: false, notified: [] })]);
    expect(screen.getByText("Ran")).toBeInTheDocument();
    expect(screen.getByText("Condition not met")).toBeInTheDocument();
    expect(screen.getByText("Not sent — state unchanged")).toBeInTheDocument();
  });

  it("shows the ⓘ button and expands the reason for an unevaluable row", async () => {
    const user = userEvent.setup();
    const onToggleError = vi.fn();
    const { rerender } = renderTable(
      [baseRun({ id: 7, condition_met: null, error: "No column is configured for this monitor." })],
      { onToggleError },
    );

    const infoButton = screen.getByRole("button", { name: "Show reason" });
    await user.click(infoButton);
    expect(onToggleError).toHaveBeenCalledWith(7);

    rerender(
      <RunsTable
        entityType="monitor"
        runs={[baseRun({ id: 7, condition_met: null, error: "No column is configured for this monitor." })]}
        now={Date.parse("2026-08-05T00:10:00Z")}
        openErrorRunId={7}
        onToggleError={onToggleError}
        onRetry={vi.fn()}
        canRetry
      />,
    );
    expect(
      screen.getByText("No column is configured for this monitor."),
    ).toBeInTheDocument();
  });

  it("still treats a failed run distinctly, with its own aria-label", () => {
    renderTable([baseRun({ status: "failed", error: "connection refused" })]);
    expect(screen.getByText("Failed to run")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show error" })).toBeInTheDocument();
  });

  it("states the first-row rule in the check-history footer", () => {
    renderTable([baseRun()]);
    expect(screen.getByText(/each check reads the first row of the result/i)).toBeInTheDocument();
  });

  it("renders a Notification failure and a header row naming the three columns", () => {
    renderTable([
      baseRun({
        condition_met: true,
        notified: [{ destination_name: "Email (you)", status: "failed", error: "boom" }],
      }),
    ]);
    expect(screen.getByText("Failed: Email (you)")).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Check" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Result" })).toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Notification" })).toBeInTheDocument();
  });

  it("renders both a sent and a failed segment when one destination delivered and a sibling failed", () => {
    renderTable([
      baseRun({
        condition_met: true,
        notified: [
          { destination_name: "Team Email", status: "sent", error: null },
          { destination_name: "Ops Webhook", status: "failed", error: "connect timed out" },
        ],
      }),
    ]);
    expect(screen.getByText(/Sent to Team Email/)).toBeInTheDocument();
    expect(screen.getByText("Failed: Ops Webhook")).toBeInTheDocument();
  });

  it("does not hide a still-retrying delivery behind a sent one", () => {
    renderTable([
      baseRun({
        condition_met: true,
        notified: [
          { destination_name: "Team Email", status: "sent", error: null },
          { destination_name: "Ops Webhook", status: "retrying", error: null },
        ],
      }),
    ]);
    // Both the sent email and the still-retrying webhook must be visible in the cell.
    expect(screen.getByText(/Sent to Team Email/)).toBeInTheDocument();
    expect(screen.getByText(/Sending to Ops Webhook/)).toBeInTheDocument();
  });

  it("includes the error text in the tooltip when present", () => {
    renderTable([
      baseRun({
        condition_met: true,
        notified: [{ destination_name: "Ops Webhook", status: "failed", error: "connect timed out" }],
      }),
    ]);
    const cell = screen.getByTitle("Ops Webhook: failed (connect timed out)");
    expect(cell).toHaveTextContent("Failed: Ops Webhook");
  });
});

describe("RunsTable — status labels", () => {
  it("renders a query-run's non-success statuses with real, distinct labels", () => {
    renderTable(
      [
        baseRun({ id: 1, status: "failed" }),
        baseRun({ id: 2, status: "timed_out" }),
        baseRun({ id: 3, status: "cancelled" }),
        baseRun({ id: 4, status: "skipped" }),
      ],
      { entityType: "query" },
    );
    expect(screen.getByText("Failed")).toBeInTheDocument();
    expect(screen.getByText("Timed out")).toBeInTheDocument();
    expect(screen.getByText("Cancelled")).toBeInTheDocument();
    expect(screen.getByText("Skipped")).toBeInTheDocument();
  });

  it("falls back to the raw status text for an unrecognized value, not 'Skipped'", () => {
    renderTable([baseRun({ id: 1, status: "some_future_status" })], { entityType: "query" });
    expect(screen.getByText("some_future_status")).toBeInTheDocument();
    expect(screen.queryByText("Skipped")).not.toBeInTheDocument();
  });

  it("treats a timed_out run as a failure: destructive label and an expandable error", () => {
    renderTable([baseRun({ status: "timed_out", error: "query exceeded the time limit" })]);
    expect(screen.getByText("Timed out")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show error" })).toBeInTheDocument();
  });

  it("does not give a cancelled run the failure treatment", () => {
    renderTable([baseRun({ status: "cancelled" })]);
    expect(screen.getByText("Cancelled")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show error" })).not.toBeInTheDocument();
  });
});
