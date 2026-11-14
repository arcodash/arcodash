import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { TileFreshnessDot } from "@/components/dashboard/TileFreshnessDot";
import type { WidgetFreshness } from "@/types/api";

const NOW = new Date("2026-07-15T12:00:00.000Z");

function minutesAgoIso(minutes: number): string {
  return new Date(NOW.getTime() - minutes * 60_000).toISOString();
}

function renderDot(freshness: WidgetFreshness) {
  return render(<TileFreshnessDot freshness={freshness} />);
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(NOW);
});

afterEach(() => {
  vi.useRealTimers();
});

describe("TileFreshnessDot", () => {
  it("a fresh success at rest shows the age text", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(8),
        expected_interval_secs: 900, // 15 min
        last_run_status: "success",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.textContent).toBe("Updated 8 min ago");
  });

  it("fresh success shows 'Updated 8 min ago' inline", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(8),
        expected_interval_secs: 900,
        last_run_status: "success",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.textContent).toBe("Updated 8 min ago");
  });

  it("fresh success always carries the exact timestamp in the title", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(8),
        expected_interval_secs: 900,
        last_run_status: "success",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.getAttribute("title")).toContain("Updated 8 min ago");
    expect(wrapper.getAttribute("title")).toMatch(/\d{4}/); // exactTs renders a locale date/time
  });

  it("goes amber with the age visible when older than 1.5x the expected interval", () => {
    const { container } = renderDot(
      {
        // 900s cadence; 1.5x = 1350s = 22.5 min. 30 min old is past that.
        retrieved_at: minutesAgoIso(30),
        expected_interval_secs: 900,
        last_run_status: "success",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).toContain("warning");
    expect(wrapper.textContent).toBe("30 min ago · expected every 15 min");
  });

  it("unscheduled (expected_interval_secs null) never goes amber no matter how old — stays neutral", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(60 * 24 * 30), // 30 days old
        expected_interval_secs: null,
        last_run_status: "success",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).not.toContain("warning");
    expect(wrapper.className).not.toContain("destructive");
    expect(wrapper.textContent).toBe("Updated 30d ago");
  });

  it("shows red 'Data may be outdated · age' when last_run_status is failed, even though retrieved_at is fresh", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(2),
        expected_interval_secs: 900,
        last_run_status: "failed",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).toContain("destructive");
    expect(wrapper.textContent).toBe("Data may be outdated · 2 min ago");
  });

  it("failed state keeps the last-good age in the title ('Showing last successful result from …')", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(180),
        expected_interval_secs: 900,
        last_run_status: "failed",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.getAttribute("title")).toContain("Showing last successful result from 3h ago");
  });

  it("failed shows the last-good age inline", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(180),
        expected_interval_secs: 900,
        last_run_status: "failed",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.textContent).toBe("Data may be outdated · 3h ago");
  });

  it("failed beats stale — a stale age still renders red, never amber", () => {
    const { container } = renderDot(
      {
        // 900s cadence, 30 min old is well past 1.5x — would be amber if not for the failed run.
        retrieved_at: minutesAgoIso(30),
        expected_interval_secs: 900,
        last_run_status: "failed",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).toContain("destructive");
    expect(wrapper.className).not.toContain("warning");
  });

  it("a timed_out last run is treated as a failure — red dot, not the green success state", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(2),
        expected_interval_secs: 900,
        last_run_status: "timed_out",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).toContain("destructive");
    expect(wrapper.textContent).toBe("Data may be outdated · 2 min ago");
  });

  it("a cancelled last run stays neutral — it's an intentional stop, not a failure", () => {
    const { container } = renderDot(
      {
        retrieved_at: minutesAgoIso(2),
        expected_interval_secs: 900,
        last_run_status: "cancelled",
      },
    );
    const wrapper = container.querySelector("span[title]")!;
    expect(wrapper.className).not.toContain("destructive");
    expect(wrapper.className).not.toContain("warning");
  });

  it("shows the age text on private dashboards too", () => {
    renderDot(
      { retrieved_at: minutesAgoIso(12), expected_interval_secs: null, last_run_status: "success" },
    );
    expect(screen.getByText(/Updated 12 min ago/)).toBeInTheDocument();
  });

  it("names the expected cadence when stale", () => {
    renderDot(
      { retrieved_at: minutesAgoIso(120), expected_interval_secs: 3600, last_run_status: "success" },
    );
    expect(screen.getByText(/2h ago · expected hourly/)).toBeInTheDocument();
  });
});
