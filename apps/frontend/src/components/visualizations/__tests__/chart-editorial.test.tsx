import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { ChartRenderer, chartDefaultOptions, fitPieLabel, formatPieTotal, INK_MUTED, PALETTE } from "../chart";
import { CounterRenderer } from "../counter";
import type { QueryResultData } from "../registry";
import { setupRechartsJsdom } from "./rechartsTestSetup";

setupRechartsJsdom();

const cols = (y = "integer"): QueryResultData["columns"] => [
  { name: "k", friendly_name: "K", type: "string" },
  { name: "v", friendly_name: "V", type: y },
];
const pie = (rows: Record<string, unknown>[], extra: Record<string, unknown> = {}) =>
  render(
    <ChartRenderer
      data={{ columns: cols("float"), rows }}
      options={{ ...chartDefaultOptions, globalSeriesType: "pie", xColumn: "k", yColumns: ["v"], legend: false, ...extra }}
    />,
  );

/** WCAG relative-luminance contrast against white. */
function contrastOnWhite(hex: string): number {
  const [r, g, b] = [1, 3, 5]
    .map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((c) => (c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4));
  return 1.05 / (0.2126 * r + 0.7152 * g + 0.0722 * b + 0.05);
}

describe("editorial chart style", () => {
  it("every palette colour meets 3:1 against white", () => {
    for (const c of PALETTE) expect(contrastOnWhite(c)).toBeGreaterThanOrEqual(3);
  });

  it("donut centre shows the rounded total, without float noise", async () => {
    pie([{ k: "a", v: 19.99 }, { k: "b", v: 19.99 }, { k: "c", v: 19.99 }]);
    await waitFor(() => expect(screen.getByText("59.97")).toBeInTheDocument());
    expect(screen.getByText("Total")).toBeInTheDocument();
  });

  it("formatPieTotal goes compact for large totals", () => {
    expect(formatPieTotal(46224)).toBe("46,224");
    expect(formatPieTotal(1234567)).toBe("1.2M");
    expect(formatPieTotal(0.1 + 0.2)).toBe("0.3");
    expect(formatPieTotal(99999.99)).toBe("100,000");
  });

  it("labels slices as 'NN% name' in muted ink and skips slices under 5%", async () => {
    pie([{ k: "big", v: 97 }, { k: "tiny", v: 3 }]);
    const label = await screen.findByText("97% big");
    expect(label.getAttribute("fill")).toBe(INK_MUTED);
    expect(screen.queryByText(/% tiny/)).not.toBeInTheDocument();
  });

  it("fitPieLabel degrades name -> percent -> nothing as room shrinks", () => {
    expect(fitPieLabel("24%", "Mid-market", 200)).toBe("24% Mid-market");
    expect(fitPieLabel("24%", "Mid-market", 40)).toBe("24%");
    expect(fitPieLabel("24%", "Mid-market", 10)).toBeNull();
  });

  it("drops slice labels for crowded pies", async () => {
    // 8 slices still get labels (proves the 9-slice check below isn't vacuous).
    const eight = pie(Array.from({ length: 8 }, (_, i) => ({ k: `s${i}`, v: 10 })));
    await screen.findByText("13% s0");
    eight.unmount();
    pie(Array.from({ length: 9 }, (_, i) => ({ k: `s${i}`, v: 10 })));
    await screen.findByText("Total");
    expect(screen.queryByText(/% s0/)).not.toBeInTheDocument();
  });

  it("mixed-sign pie drops non-positive rows from slices and total", async () => {
    pie([{ k: "a", v: 30 }, { k: "neg", v: -10 }, { k: "b", v: 10 }]);
    await screen.findByText("75% a");
    expect(screen.getByText("40")).toBeInTheDocument();
    expect(screen.queryByText(/neg/)).not.toBeInTheDocument();
  });

  it("negative-only pie shows an explicit empty state", () => {
    pie([{ k: "a", v: -5 }, { k: "b", v: -1 }]);
    expect(screen.getByText("A pie chart needs positive values.")).toBeInTheDocument();
  });

  it("legend text uses muted ink, not the series colour", async () => {
    render(
      <ChartRenderer
        data={{ columns: cols(), rows: [{ k: "a", v: 1 }] }}
        options={{ ...chartDefaultOptions, xColumn: "k", yColumns: ["v"], legend: true }}
      />,
    );
    const item = await screen.findByText("v", { selector: ".recharts-legend-item-text span" });
    expect(item).toHaveStyle({ color: INK_MUTED });
  });

  it("line dots show for short series and disappear past 24 points", () => {
    const line = (n: number) =>
      render(
        <ChartRenderer
          data={{ columns: cols(), rows: Array.from({ length: n }, (_, i) => ({ k: `p${i}`, v: i })) }}
          options={{ ...chartDefaultOptions, globalSeriesType: "line", xColumn: "k", yColumns: ["v"] }}
        />,
      );
    const short = line(5);
    expect(short.container.querySelectorAll(".recharts-line-dot").length).toBe(5);
    short.unmount();
    const long = line(25);
    expect(long.container.querySelectorAll(".recharts-line-dot").length).toBe(0);
  });
});

describe("editorial counter", () => {
  const data: QueryResultData = { columns: cols(), rows: [{ k: "a", v: 13 }] };

  it("renders prefix and suffix as separate small spans", () => {
    render(<CounterRenderer data={data} options={{ columnName: "v", stringPrefix: "$", stringSuffix: "k" }} />);
    const value = screen.getByTestId("counter-value");
    expect(value).toHaveTextContent("$13k");
    const spans = value.querySelectorAll("span");
    expect([...spans].map((s) => s.textContent)).toEqual(["$", "k"]);
  });

  it("omits prefix/suffix spans when there is no value", () => {
    render(
      <CounterRenderer data={{ columns: cols(), rows: [] }} options={{ columnName: "v", stringPrefix: "$", stringSuffix: "k" }} />,
    );
    expect(screen.getByTestId("counter-value").querySelectorAll("span").length).toBe(0);
  });
});
