import { describe, it, expect, vi, beforeEach } from "vitest";
import type * as Recharts from "recharts";
import { render } from "@testing-library/react";
import { ChartRenderer, chartDefaultOptions } from "../chart";
import type { QueryResultData } from "../registry";
import { setupRechartsJsdom } from "./rechartsTestSetup";

setupRechartsJsdom();

const seen = vi.hoisted(() => ({ props: {} as Record<string, unknown[]>, xAxis: [] as Record<string, unknown>[] }));

vi.mock("recharts", async () => {
  const actual = await vi.importActual<typeof Recharts>("recharts");
  const spy = (name: string, Comp: React.ComponentType<Record<string, unknown>>) => {
    const Wrapped = (props: Record<string, unknown>) => {
      (seen.props[name] ??= []).push(props.isAnimationActive);
      return <Comp {...props} />;
    };
    // Recharts discovers marks by displayName/static fields; carry them over.
    return Object.assign(Wrapped, Comp);
  };
  return {
    ...actual,
    Bar: spy("bar", actual.Bar as never),
    Line: spy("line", actual.Line as never),
    Area: spy("area", actual.Area as never),
    Scatter: spy("scatter", actual.Scatter as never),
    Pie: spy("pie", actual.Pie as never),
    XAxis: Object.assign((props: Record<string, unknown>) => {
      seen.xAxis.push({ type: props.type, domain: props.domain });
      const X = actual.XAxis as unknown as React.ComponentType<Record<string, unknown>>;
      return <X {...props} />;
    }, actual.XAxis),
  };
});

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

describe("ChartRenderer static draw", () => {
  beforeEach(() => {
    seen.props = {};
    seen.xAxis = [];
  });

  it.each(["bar", "line", "area", "scatter", "pie"])("%s marks disable mount animation", (chartType) => {
    render(<ChartRenderer data={data} options={{ ...chartDefaultOptions, globalSeriesType: chartType as never, xColumn: "day", yColumns: ["count"] }} />);
    expect(seen.props[chartType]?.length).toBeGreaterThan(0);
    expect(seen.props[chartType]!.every((v) => v === false)).toBe(true);
  });

  it("numeric scatter X gets a number axis with an auto domain (not 0-anchored, one axis for all series)", () => {
    const years: QueryResultData = {
      columns: [
        { name: "year", friendly_name: "Year", type: "integer" },
        { name: "v", friendly_name: "V", type: "integer" },
      ],
      rows: [2020, 2021, 2022].map((year, i) => ({ year, v: i })),
    };
    render(<ChartRenderer data={years} options={{ ...chartDefaultOptions, globalSeriesType: "scatter", xColumn: "year", yColumns: ["v"] }} />);
    expect(seen.xAxis.at(-1)).toEqual({ type: "number", domain: ["auto", "auto"] });
  });

  it("string scatter X stays a category axis", () => {
    render(<ChartRenderer data={data} options={{ ...chartDefaultOptions, globalSeriesType: "scatter", xColumn: "day", yColumns: ["count"] }} />);
    expect(seen.xAxis.at(-1)).toEqual({ type: "category", domain: undefined });
  });
});
