import { describe, it, expect } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { ChartRenderer, chartDefaultOptions, aggregateByX, pivotBySeries } from "../chart";
import type { QueryResultData } from "../registry";
import { setupRechartsJsdom } from "./rechartsTestSetup";

setupRechartsJsdom();

const data: QueryResultData = {
  columns: [
    { name: "day", friendly_name: "Day", type: "string" },
    { name: "count", friendly_name: "Count", type: "integer" },
  ],
  rows: [
    { day: "Mon", count: 1 },
    { day: "Tue", count: 2 },
  ],
};

describe("ChartRenderer", () => {
  it("renders without crashing given minimal data and options", () => {
    const { container } = render(
      <ChartRenderer data={data} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"] }} />,
    );
    expect(container.querySelector(".recharts-responsive-container")).toBeTruthy();
  });

  it("shows the no-columns empty state when the result has no columns at all", () => {
    const { getByText } = render(<ChartRenderer data={{ columns: [], rows: [] }} options={{}} />);
    expect(getByText("No columns in this result")).toBeInTheDocument();
  });

  it("default options (empty-string column fields) still render real output, not an empty state", () => {
    // Fresh chart defaults (xColumn "", yColumns []) must fall back to the first/second column.
    render(<ChartRenderer data={data} options={chartDefaultOptions} />);
    expect(screen.queryByText("Pick a column to plot")).not.toBeInTheDocument();
    expect(document.querySelector(".recharts-responsive-container")).toBeTruthy();
  });

  it("a string column is never inferred as the default Y series", () => {
    const storeData: QueryResultData = {
      columns: [
        { name: "region", friendly_name: "Region", type: "string" },
        { name: "net_revenue", friendly_name: "Net revenue", type: "float" },
      ],
      rows: [
        { region: "East", net_revenue: "100.00" },
        { region: "West", net_revenue: "200.00" },
      ],
    };
    const { container } = render(<ChartRenderer data={storeData} options={chartDefaultOptions} />);
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(2);
    expect(screen.getByText("net_revenue")).toBeInTheDocument();
    expect(screen.queryByText("region")).not.toBeInTheDocument();
  });

  it("a result with no numeric column at all gets the no-number-column empty state, not an invented series", () => {
    const allStringData: QueryResultData = {
      columns: [
        { name: "region", friendly_name: "Region", type: "string" },
        { name: "status", friendly_name: "Status", type: "string" },
      ],
      rows: [{ region: "East", status: "ok" }],
    };
    render(<ChartRenderer data={allStringData} options={chartDefaultOptions} />);
    expect(screen.getByText("No number column to plot")).toBeInTheDocument();
  });

  it("defaults to a bar chart for categorical comparison data (a line would falsely imply trend between unrelated categories)", () => {
    const categoricalData: QueryResultData = {
      columns: [
        { name: "category", friendly_name: "Category", type: "string" },
        { name: "revenue", friendly_name: "Revenue", type: "float" },
      ],
      rows: [
        { category: "Apparel", revenue: 1200 },
        { category: "Sports & Outdoors", revenue: 900 },
      ],
    };
    const { container } = render(<ChartRenderer data={categoricalData} options={chartDefaultOptions} />);
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(2);
    expect(container.querySelectorAll(".recharts-line-curve").length).toBe(0);
  });

  it("still defaults to a line chart when the x-axis is a date column (a continuous/ordered axis)", () => {
    const timeSeriesData: QueryResultData = {
      columns: [
        { name: "day", friendly_name: "Day", type: "date" },
        { name: "revenue", friendly_name: "Revenue", type: "float" },
      ],
      rows: [
        { day: "2026-08-01", revenue: 1200 },
        { day: "2026-08-02", revenue: 900 },
      ],
    };
    const { container } = render(<ChartRenderer data={timeSeriesData} options={chartDefaultOptions} />);
    expect(container.querySelectorAll(".recharts-line-curve").length).toBe(1);
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(0);
  });

  it("an explicitly-emptied Y config shows the pick-a-column empty state, not an inferred chart", () => {
    render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, xColumn: "day", yColumns: [], yColumnsTouched: true }}
      />,
    );
    expect(screen.getByText("Pick a column to plot")).toBeInTheDocument();
    expect(document.querySelector(".recharts-responsive-container")).not.toBeInTheDocument();
  });

  it("bar chart renders a bar per row", () => {
    const { container } = render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "bar", xColumn: "day", yColumns: ["count"] }}
      />,
    );
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(data.rows.length);
  });

  it("area chart renders an area mark", () => {
    const { container } = render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "area", xColumn: "day", yColumns: ["count"] }}
      />,
    );
    expect(container.querySelectorAll(".recharts-area-area").length).toBe(1);
  });

  it("line chart renders a line mark", () => {
    const { container } = render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "line", xColumn: "day", yColumns: ["count"] }}
      />,
    );
    expect(container.querySelectorAll(".recharts-line-curve").length).toBe(1);
  });

  it(
    "pie chart mounts a recharts Pie with the right slice count",
    async () => {
      const { container } = render(
        <ChartRenderer
          data={data}
          options={{ ...chartDefaultOptions, globalSeriesType: "pie", xColumn: "day", yColumns: ["count"] }}
        />,
      );
      expect(container.querySelectorAll(".recharts-pie").length).toBe(1);
      // Pie slices paint one frame after mount (react-smooth).
      await waitFor(
        () => expect(container.querySelectorAll(".recharts-pie-sector").length).toBe(data.rows.length),
        // Generous headroom: react-smooth's first frame can land late on a contended full-suite
        // run.
        { timeout: 18_000 },
      );
    },
    // Above waitFor's timeout; vitest's 5000ms default would kill the test first.
    20_000,
  );

  it("scatter chart renders a symbol per row", () => {
    const { container } = render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "scatter", xColumn: "day", yColumns: ["count"] }}
      />,
    );
    expect(container.querySelectorAll(".recharts-scatter-symbol").length).toBe(data.rows.length);
  });

  it("legend renders when legend is true, and is omitted when false", () => {
    const withLegend = render(
      <ChartRenderer data={data} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"], legend: true }} />,
    );
    expect(withLegend.container.querySelector(".recharts-legend-wrapper")).toBeTruthy();
    withLegend.unmount();

    const withoutLegend = render(
      <ChartRenderer data={data} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"], legend: false }} />,
    );
    expect(withoutLegend.container.querySelector(".recharts-legend-wrapper")).toBeFalsy();
  });

  it("stacking: percent formats the y-axis as percentage ticks", () => {
    const { container } = render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "bar", xColumn: "day", yColumns: ["count"], stacking: "percent" }}
      />,
    );
    const yTicks = Array.from(
      container.querySelectorAll(".recharts-yAxis-tick-labels .recharts-cartesian-axis-tick-value"),
    ).map((n) => n.textContent);
    expect(yTicks.length).toBeGreaterThan(0);
    expect(yTicks.every((t) => /%$/.test(t ?? ""))).toBe(true);
  });

  it("renders two y-series as two separate marks", () => {
    const twoSeriesData: QueryResultData = {
      columns: [
        { name: "day", friendly_name: "Day", type: "string" },
        { name: "a", friendly_name: "A", type: "integer" },
        { name: "b", friendly_name: "B", type: "integer" },
      ],
      rows: [
        { day: "Mon", a: 1, b: 5 },
        { day: "Tue", a: 2, b: 6 },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={twoSeriesData}
        options={{ ...chartDefaultOptions, globalSeriesType: "line", xColumn: "day", yColumns: ["a", "b"] }}
      />,
    );
    expect(container.querySelectorAll(".recharts-line-curve").length).toBe(2);
    expect(screen.getByText("a")).toBeInTheDocument();
    expect(screen.getByText("b")).toBeInTheDocument();
  });

  it("no crash when yColumns is missing and no non-x column exists to infer", () => {
    const singleColData: QueryResultData = {
      columns: [{ name: "day", friendly_name: "Day", type: "string" }],
      rows: [{ day: "Mon" }],
    };
    render(<ChartRenderer data={singleColData} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: [] }} />);
    expect(screen.getByText("No number column to plot")).toBeInTheDocument();
  });

  it("numeric-string y-column rows (Postgres NUMERIC serialized as JSON string) render a real numeric axis", () => {
    // Bar/stack math coerces numeric strings implicitly; only the scatter case depends on explicit
    // coercion.
    const stringNumericData: QueryResultData = {
      columns: [
        { name: "day", friendly_name: "Day", type: "string" },
        { name: "total", friendly_name: "Total", type: "string" },
      ],
      rows: [
        { day: "Mon", total: "12.50" },
        { day: "Tue", total: "7.25" },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={stringNumericData}
        options={{ ...chartDefaultOptions, globalSeriesType: "bar", xColumn: "day", yColumns: ["total"] }}
      />,
    );
    const yTicks = container.querySelectorAll(".recharts-yAxis-tick-labels .recharts-cartesian-axis-tick-value");
    expect(yTicks.length).toBeGreaterThan(0);
    for (const tick of yTicks) {
      expect(tick.textContent).not.toBe("NaN");
      expect(tick.textContent?.trim()).not.toBe("");
    }
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(2);
  });

  it("numeric-string x/y on a scatter chart render distinct real coordinates, not both collapsed to the origin", () => {
    const stringNumericData: QueryResultData = {
      columns: [
        { name: "x", friendly_name: "X", type: "string" },
        { name: "y", friendly_name: "Y", type: "string" },
      ],
      rows: [
        { x: "1", y: "10" },
        { x: "2", y: "20" },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={stringNumericData}
        options={{ ...chartDefaultOptions, globalSeriesType: "scatter", xColumn: "x", yColumns: ["y"] }}
      />,
    );
    const symbols = container.querySelectorAll(".recharts-scatter-symbol path");
    expect(symbols.length).toBe(2);
    // Without coercion Recharts can't build a numeric scale and every point collapses onto one
    // cx/cy.
    const positions = Array.from(symbols).map((s) => `${s.getAttribute("cx")},${s.getAttribute("cy")}`);
    expect(new Set(positions).size).toBe(2);
    for (const pos of positions) {
      expect(pos).not.toBe("0,0");
      expect(pos).not.toContain("NaN");
    }
  });

  it("Y-axis ticks use the shared number formatter, not raw digits", () => {
    const bigData: QueryResultData = {
      columns: [
        { name: "month", friendly_name: "Month", type: "string" },
        { name: "revenue", friendly_name: "Revenue", type: "integer" },
      ],
      rows: [
        { month: "Jan", revenue: 60000 },
        { month: "Feb", revenue: 30000 },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={bigData}
        options={{ ...chartDefaultOptions, globalSeriesType: "bar", xColumn: "month", yColumns: ["revenue"] }}
      />,
    );
    const yTicks = Array.from(
      container.querySelectorAll(".recharts-yAxis-tick-labels .recharts-cartesian-axis-tick-value"),
    ).map((n) => n.textContent);
    expect(yTicks.length).toBeGreaterThan(0);
    expect(yTicks.some((t) => /^\d{1,3}(,\d{3})*$/.test(t ?? ""))).toBe(true);
  });

  it("a long X-axis category label is truncated on the tick, not left to collide", () => {
    const longLabelData: QueryResultData = {
      columns: [
        { name: "store", friendly_name: "Store", type: "string" },
        { name: "revenue", friendly_name: "Revenue", type: "integer" },
      ],
      rows: [
        { store: "Camden Market Street Flagship", revenue: 10 },
        { store: "Altstadt", revenue: 20 },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={longLabelData}
        options={{ ...chartDefaultOptions, globalSeriesType: "bar", xColumn: "store", yColumns: ["revenue"] }}
      />,
    );
    const xTicks = Array.from(
      container.querySelectorAll(".recharts-xAxis-tick-labels .recharts-cartesian-axis-tick-value"),
    ).map((n) => n.textContent ?? "");
    expect(xTicks.length).toBeGreaterThan(0);
    for (const t of xTicks) expect(t.length).toBeLessThanOrEqual(14);
    expect(xTicks.some((t) => t.endsWith("…"))).toBe(true);
  });

  it("a date x-axis renders formatted tick labels ('Jul 2022'), not raw ISO timestamps", () => {
    const dateData: QueryResultData = {
      columns: [
        { name: "month", friendly_name: "Month", type: "date" },
        { name: "revenue", friendly_name: "Revenue", type: "integer" },
      ],
      rows: [
        { month: "2022-07-01T00:00:00Z", revenue: 100 },
        { month: "2022-08-01T00:00:00Z", revenue: 200 },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={dateData}
        options={{ ...chartDefaultOptions, globalSeriesType: "line", xColumn: "month", yColumns: ["revenue"] }}
      />,
    );
    const xTicks = Array.from(
      container.querySelectorAll(".recharts-xAxis-tick-labels .recharts-cartesian-axis-tick-value"),
    ).map((n) => n.textContent ?? "");
    expect(xTicks.length).toBeGreaterThan(0);
    expect(xTicks).toContain("Jul 2022");
    for (const t of xTicks) expect(t).not.toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });

  it("group by pivots one value column into one series per group value", () => {
    const storeMonthData: QueryResultData = {
      columns: [
        { name: "month", friendly_name: "Month", type: "string" },
        { name: "store", friendly_name: "Store", type: "string" },
        { name: "net_revenue", friendly_name: "Net revenue", type: "float" },
      ],
      rows: [
        { month: "Jan", store: "A", net_revenue: "100" },
        { month: "Jan", store: "B", net_revenue: "200" },
        { month: "Feb", store: "A", net_revenue: "150" },
        { month: "Feb", store: "B", net_revenue: "250" },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={storeMonthData}
        options={{
          ...chartDefaultOptions,
          globalSeriesType: "line",
          xColumn: "month",
          yColumns: ["net_revenue"],
          seriesColumn: "store",
          aggregation: "sum",
        }}
      />,
    );
    expect(container.querySelectorAll(".recharts-line-curve").length).toBe(2);
    expect(screen.getByText("A")).toBeInTheDocument();
    expect(screen.getByText("B")).toBeInTheDocument();
    expect(screen.queryByText("net_revenue")).not.toBeInTheDocument();
  });

  it("group by combines duplicate (x, group) rows by the chosen aggregation", () => {
    const dupData: QueryResultData = {
      columns: [
        { name: "month", friendly_name: "Month", type: "string" },
        { name: "store", friendly_name: "Store", type: "string" },
        { name: "units", friendly_name: "Units", type: "integer" },
      ],
      rows: [
        { month: "Jan", store: "A", units: 3 },
        { month: "Jan", store: "A", units: 4 },
      ],
    };
    const { container } = render(
      <ChartRenderer
        data={dupData}
        options={{
          ...chartDefaultOptions,
          globalSeriesType: "bar",
          xColumn: "month",
          yColumns: ["units"],
          seriesColumn: "store",
          aggregation: "sum",
        }}
      />,
    );
    expect(container.querySelectorAll(".recharts-bar-rectangle").length).toBe(1);
  });

  it("a group by with no aggregation chosen still combines (forced to sum)", () => {
    const dupData: QueryResultData = {
      columns: [
        { name: "month", friendly_name: "Month", type: "string" },
        { name: "store", friendly_name: "Store", type: "string" },
        { name: "units", friendly_name: "Units", type: "integer" },
      ],
      rows: [
        { month: "Jan", store: "A", units: 3 },
        { month: "Jan", store: "A", units: 4 },
      ],
    };
    render(
      <ChartRenderer
        data={dupData}
        options={{
          ...chartDefaultOptions,
          globalSeriesType: "bar",
          xColumn: "month",
          yColumns: ["units"],
          seriesColumn: "store",
          aggregation: "none",
        }}
      />,
    );
    expect(document.querySelector(".recharts-responsive-container")).toBeTruthy();
  });

  it("count aggregation labels the series 'Count', not the Y column's own name", () => {
    const rows = [
      { month: "Jan", net_revenue: 100 },
      { month: "Jan", net_revenue: 200 },
      { month: "Feb", net_revenue: 50 },
    ];
    render(
      <ChartRenderer
        data={{
          columns: [
            { name: "month", friendly_name: "Month", type: "string" },
            { name: "net_revenue", friendly_name: "Net revenue", type: "float" },
          ],
          rows,
        }}
        options={{
          ...chartDefaultOptions,
          globalSeriesType: "bar",
          xColumn: "month",
          yColumns: ["net_revenue"],
          aggregation: "count",
        }}
      />,
    );
    expect(screen.getByText("Count")).toBeInTheDocument();
    expect(screen.queryByText("net_revenue")).not.toBeInTheDocument();
  });

  it("pie chart with an all-zero value column shows the nothing-to-plot empty state", () => {
    const zeroData: QueryResultData = {
      columns: [
        { name: "region", friendly_name: "Region", type: "string" },
        { name: "revenue", friendly_name: "Revenue", type: "integer" },
      ],
      rows: [
        { region: "East", revenue: 0 },
        { region: "West", revenue: 0 },
      ],
    };
    render(
      <ChartRenderer
        data={zeroData}
        options={{ ...chartDefaultOptions, globalSeriesType: "pie", xColumn: "region", yColumns: ["revenue"] }}
      />,
    );
    expect(screen.getByText("Nothing to plot")).toBeInTheDocument();
    expect(screen.getByText("Every value in this column is zero.")).toBeInTheDocument();
  });

  it("renders a title above the plot only when a title prop is passed", () => {
    const withoutTitle = render(
      <ChartRenderer data={data} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"] }} />,
    );
    expect(withoutTitle.queryByText("Daily counts")).not.toBeInTheDocument();
    withoutTitle.unmount();

    render(
      <ChartRenderer
        data={data}
        options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"] }}
        title="Daily counts"
      />,
    );
    expect(screen.getByText("Daily counts")).toBeInTheDocument();
  });
});

describe("aggregateByX/pivotBySeries: computed values, not just mark counts", () => {
  const dupRows = [
    { month: "Jan", units: "100.50" },
    { month: "Jan", units: "200.25" },
  ];

  it("sum: aggregateByX adds numeric-string values for the same X", () => {
    const out = aggregateByX(dupRows, "month", ["units"], "sum");
    expect(out).toEqual([{ month: "Jan", units: 300.75 }]);
  });

  it("sum: pivotBySeries adds numeric-string values for the same (x, series)", () => {
    const rows = dupRows.map((r) => ({ ...r, store: "A" }));
    const { rows: out } = pivotBySeries(rows, "month", "units", "store", "sum");
    expect(out).toEqual([{ month: "Jan", A: 300.75 }]);
  });

  it("avg: aggregateByX averages values sharing an X", () => {
    const rows = [
      { month: "Jan", units: "100" },
      { month: "Jan", units: "200" },
    ];
    expect(aggregateByX(rows, "month", ["units"], "avg")).toEqual([{ month: "Jan", units: 150 }]);
  });

  it("min/max: aggregateByX takes the extreme, not a Math.min/max spread artifact", () => {
    const rows = [
      { month: "Jan", units: "100" },
      { month: "Jan", units: "200" },
      { month: "Jan", units: "50" },
    ];
    expect(aggregateByX(rows, "month", ["units"], "min")).toEqual([{ month: "Jan", units: 50 }]);
    expect(aggregateByX(rows, "month", ["units"], "max")).toEqual([{ month: "Jan", units: 200 }]);
  });

  it("count: aggregateByX counts rows, independent of the values in them", () => {
    const rows = [
      { month: "Jan", units: "100" },
      { month: "Jan", units: "200" },
      { month: "Feb", units: "9" },
    ];
    expect(aggregateByX(rows, "month", ["units"], "count")).toEqual([
      { month: "Jan", units: 2 },
      { month: "Feb", units: 1 },
    ]);
  });

  it("null values are skipped, not coerced to 0 (avg/min unaffected by a NULL in the bucket)", () => {
    const rows = [
      { month: "Jan", units: 100 },
      { month: "Jan", units: 200 },
      { month: "Jan", units: null },
    ];
    // Number(null) is 0 — a naive fix would give avg=100, min=0. The correct answer skips it.
    expect(aggregateByX(rows, "month", ["units"], "avg")).toEqual([{ month: "Jan", units: 150 }]);
    expect(aggregateByX(rows, "month", ["units"], "min")).toEqual([{ month: "Jan", units: 100 }]);
  });

  it("pivotBySeries: avg/min/max/count per (x, series) group", () => {
    const rows = [
      { month: "Jan", store: "A", units: "10" },
      { month: "Jan", store: "A", units: "30" },
      { month: "Jan", store: "B", units: "5" },
    ];
    expect(pivotBySeries(rows, "month", "units", "store", "avg").rows).toEqual([
      { month: "Jan", A: 20, B: 5 },
    ]);
    expect(pivotBySeries(rows, "month", "units", "store", "min").rows).toEqual([
      { month: "Jan", A: 10, B: 5 },
    ]);
    expect(pivotBySeries(rows, "month", "units", "store", "max").rows).toEqual([
      { month: "Jan", A: 30, B: 5 },
    ]);
    expect(pivotBySeries(rows, "month", "units", "store", "count").rows).toEqual([
      { month: "Jan", A: 2, B: 1 },
    ]);
  });

  it("pivotBySeries: a NULL in the value column is skipped, not zeroed", () => {
    const rows = [
      { month: "Jan", store: "A", units: 100 },
      { month: "Jan", store: "A", units: null },
    ];
    expect(pivotBySeries(rows, "month", "units", "store", "avg").rows).toEqual([
      { month: "Jan", A: 100 },
    ]);
  });
});
