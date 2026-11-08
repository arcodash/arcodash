import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ObservationSparkline } from "../ObservationSparkline";
import { median } from "@/lib/monitors";
import type { MonitorObservation } from "@/types/api";

const obs = (value: number | null, i = 0): MonitorObservation => ({
  observed_at: `2026-01-0${i + 1}T00:00:00Z`,
  value,
  state: "ok",
  explanation: null,
  run_id: null,
});

const base = { kind: "baseline" as const, column: "backlog" };

describe("ObservationSparkline states", () => {
  it("renders the empty, single and all-null states", () => {
    const { rerender } = render(<ObservationSparkline {...base} observations={[]} />);
    expect(screen.getByText(/No checks yet/)).toBeInTheDocument();

    rerender(<ObservationSparkline {...base} observations={[obs(42)]} />);
    expect(screen.getByText("1 check so far: 42")).toBeInTheDocument();

    rerender(<ObservationSparkline {...base} observations={[obs(null, 0), obs(null, 1)]} />);
    expect(screen.getByText("Recent checks couldn't read a number from backlog.")).toBeInTheDocument();
  });

  it("offers a retry on error", () => {
    render(<ObservationSparkline {...base} observations={undefined} isError />);
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });
});

describe("median", () => {
  it("handles odd, even and empty input", () => {
    expect(median([3, 1, 2])).toBe(2);
    expect(median([4, 1, 2, 3])).toBe(2.5);
    expect(median([])).toBeNull();
  });
});

describe("ObservationSparkline learning", () => {
  const learning = { have: 1, need: 7 };
  const status = (kind: "paused" | "no-eta") => ({ kind, have: 1, need: 7, label: "", ariaLabel: "" });

  it("keeps the chart and explains progress with a single check", () => {
    render(<ObservationSparkline {...base} observations={[obs(42)]} learning={learning} />);
    expect(screen.queryByText(/1 check so far/)).not.toBeInTheDocument();
    expect(screen.getByText("Learning: 1 of 7 checks. Alerts start after 7.")).toBeInTheDocument();
  });

  it("says the schedule is off when paused", () => {
    render(<ObservationSparkline {...base} observations={[obs(42), obs(43, 1)]} learning={learning} learningStatus={status("paused")} />);
    expect(screen.getByText("Learning paused: 1 of 7 checks — the check schedule is off.")).toBeInTheDocument();
  });

  it("shows the alert time and cadence when active", () => {
    const active = { ...status("no-eta"), kind: "active" as const, alertsAtMs: Date.parse("2026-01-01T15:30:00Z"), cadenceLabel: "Hourly" };
    render(<ObservationSparkline {...base} observations={[obs(42), obs(43, 1)]} learning={learning} learningStatus={active} nowMs={Date.parse("2026-01-01T12:00:00Z")} />);
    expect(screen.getByText(/Learning: 1 of 7 checks · alerts start ~\d\d:\d\d \(Hourly\)/)).toBeInTheDocument();
  });
});
