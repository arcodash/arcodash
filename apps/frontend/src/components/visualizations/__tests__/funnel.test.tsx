import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { FunnelRenderer, funnelDefaultOptions, funnelMax } from "../funnel";
import type { QueryResultData } from "../registry";

const data: QueryResultData = {
  columns: [
    { name: "stage", friendly_name: "Stage", type: "string" },
    { name: "count", friendly_name: "Count", type: "number" },
  ],
  rows: [
    { stage: "Visited", count: 100 },
    { stage: "Signed up", count: 40 },
  ],
};

describe("FunnelRenderer", () => {
  it("renders a bar per row with label and percentage given minimal data", () => {
    render(<FunnelRenderer data={data} options={{ ...funnelDefaultOptions, column: "stage", valueColumn: "count" }} />);
    expect(screen.getByText("Visited")).toBeInTheDocument();
    expect(screen.getByText("Signed up")).toBeInTheDocument();
    expect(screen.getByText("100")).toBeInTheDocument();
    expect(screen.getByText("40%")).toBeInTheDocument();
  });

  it("default options (empty column fields) fall back to the first two columns", () => {
    render(<FunnelRenderer data={data} options={funnelDefaultOptions} />);
    expect(screen.getByText("Visited")).toBeInTheDocument();
    expect(screen.getByText("40%")).toBeInTheDocument();
  });

  it("computes 100/40/12 percentages relative to the first row", () => {
    const threeStage: QueryResultData = {
      columns: [
        { name: "stage", friendly_name: "Stage", type: "string" },
        { name: "count", friendly_name: "Count", type: "number" },
      ],
      rows: [
        { stage: "Visited", count: 500 },
        { stage: "Signed up", count: 200 },
        { stage: "Purchased", count: 60 },
      ],
    };
    render(<FunnelRenderer data={threeStage} options={{ ...funnelDefaultOptions, column: "stage", valueColumn: "count" }} />);
    expect(screen.getByText("100%")).toBeInTheDocument();
    expect(screen.getByText("40%")).toBeInTheDocument();
    expect(screen.getByText("12%")).toBeInTheDocument();
  });

  it("a non-numeric value column renders 0, not the literal text NaN, and never a NaN width", () => {
    const badData: QueryResultData = {
      columns: [
        { name: "stage", friendly_name: "Stage", type: "string" },
        { name: "count", friendly_name: "Count", type: "string" },
      ],
      rows: [
        { stage: "Visited", count: "not-a-number" },
        { stage: "Signed up", count: "also-bad" },
      ],
    };
    const { container } = render(
      <FunnelRenderer data={badData} options={{ ...funnelDefaultOptions, column: "stage", valueColumn: "count" }} />,
    );
    expect(screen.queryAllByText("NaN")).toHaveLength(0);
    expect(screen.queryAllByText("NaN%")).toHaveLength(0);
    for (const bar of container.querySelectorAll<HTMLElement>("[style*='width']")) {
      expect(bar.style.width).not.toContain("NaN");
    }
  });

  it("renders a result set large enough to break a Math.max spread", () => {
    // The real engine limit (~125k args) varies with stack size and costs a 130k-node jsdom
    // render; capping Math.max's arity pins the same invariant deterministically at 2k rows.
    const realMax = Math.max;
    const maxSpy = vi.spyOn(Math, "max").mockImplementation((...args: number[]) => {
      if (args.length > 100) throw new RangeError("Maximum call stack size exceeded");
      return realMax(...args);
    });
    const rowCount = 2_000;
    const rows = Array.from({ length: rowCount }, (_, i) => ({
      stage: `Stage ${i}`,
      // Only the last row carries a nonzero value, so exactly one bar should hit 100%.
      count: i === rowCount - 1 ? 1000 : 0,
    }));
    const bigData: QueryResultData = {
      columns: [
        { name: "stage", friendly_name: "Stage", type: "string" },
        { name: "count", friendly_name: "Count", type: "number" },
      ],
      rows,
    };
    try {
      render(
        <FunnelRenderer data={bigData} options={{ ...funnelDefaultOptions, column: "stage", valueColumn: "count" }} />,
      );
      expect(screen.getByText("100%")).toBeInTheDocument();
    } finally {
      maxSpy.mockRestore();
    }
  });
});

describe("funnelMax", () => {
  it("handles an array past this engine's Math.max spread limit", () => {
    // Probe for the size at which a spread throws here; double up to a 2M cap.
    let n = 1_000;
    const spreads = (len: number) => {
      try {
        Math.max(...new Array<number>(len).fill(0));
        return false;
      } catch (e) {
        return e instanceof RangeError;
      }
    };
    while (n < 2_000_000 && !spreads(n)) n *= 2;
    const values = new Array<number>(n).fill(0);
    values[n - 1] = 1000;
    expect(funnelMax(values)).toBe(1000);
    expect(funnelMax(new Array<number>(n).fill(0))).toBe(1);
    expect(funnelMax([])).toBe(1);
  });
});
