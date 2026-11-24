import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { DashboardHealthLine } from "@/components/dashboard/DashboardHealthLine";
import type { Widget } from "@/types/api";

function viz(triggered: number, ok: number): Widget {
  return {
    id: 1,
    visualization_id: 3,
    freshness: {
      retrieved_at: new Date(Date.now() - 5 * 60_000).toISOString(),
      expected_interval_secs: null,
      last_run_status: "success",
    },
    check_summary: { total: triggered + ok, ok, triggered, unevaluable: 0 },
  } as unknown as Widget;
}

describe("DashboardHealthLine", () => {
  it("renders the line with only the failing segment red", () => {
    render(<DashboardHealthLine widgets={[viz(1, 2)]} />);
    expect(screen.getByTestId("dashboard-health-line").textContent).toBe(
      "Oldest result 5 min ago · 1 check failing · 2 passing",
    );
    expect(screen.getByText("1 check failing").className).toContain("text-destructive");
    expect(screen.getByText("2 passing").className).not.toContain("text-destructive");
  });

  it("renders nothing without freshness", () => {
    const { container } = render(<DashboardHealthLine widgets={[]} />);
    expect(container.firstChild).toBeNull();
  });
});
