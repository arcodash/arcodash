import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { CounterRenderer, counterDefaultOptions } from "../counter";
import type { QueryResultData } from "../registry";

const data: QueryResultData = {
  columns: [{ name: "total", friendly_name: "Total", type: "number" }],
  rows: [{ total: 42 }],
};

describe("CounterRenderer", () => {
  it("renders the value from the configured column", () => {
    render(<CounterRenderer data={data} options={{ ...counterDefaultOptions, columnName: "total" }} />);
    expect(screen.getByText("42")).toBeInTheDocument();
  });

  it("counts rows when countRow option is set", () => {
    render(
      <CounterRenderer
        data={{ columns: data.columns, rows: [{ total: 1 }, { total: 2 }, { total: 3 }] }}
        options={{ ...counterDefaultOptions, countRow: true }}
      />,
    );
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("default options (empty columnName) fall back to the first column", () => {
    render(<CounterRenderer data={data} options={counterDefaultOptions} />);
    expect(screen.getByText("42")).toBeInTheDocument();
  });

  it("applies prefix and suffix around the value", () => {
    render(
      <CounterRenderer
        data={data}
        options={{ ...counterDefaultOptions, columnName: "total", stringPrefix: "$", stringSuffix: " USD" }}
      />,
    );
    expect(screen.getByTestId("counter-value")).toHaveTextContent(/^\$42 USD$/);
  });

  it("reads the configured rowNumber (1-indexed)", () => {
    const rows = [{ total: 10 }, { total: 20 }, { total: 30 }];
    render(
      <CounterRenderer data={{ columns: data.columns, rows }} options={{ ...counterDefaultOptions, columnName: "total", rowNumber: 2 }} />,
    );
    expect(screen.getByText("20")).toBeInTheDocument();
  });

  it("shows an em-dash when the configured row/column has no value", () => {
    render(
      <CounterRenderer data={{ columns: data.columns, rows: [] }} options={{ ...counterDefaultOptions, columnName: "total" }} />,
    );
    expect(screen.getByText("—")).toBeInTheDocument();
  });

  it("renders a non-numeric text value as-is (no numeric coercion required)", () => {
    const textData: QueryResultData = {
      columns: [{ name: "status", friendly_name: "Status", type: "string" }],
      rows: [{ status: "Active" }],
    };
    render(<CounterRenderer data={textData} options={{ ...counterDefaultOptions, columnName: "status" }} />);
    expect(screen.getByText("Active")).toBeInTheDocument();
  });

  it("shows an up-trend (emerald) when value meets or exceeds the target", () => {
    render(
      <CounterRenderer
        data={{ columns: [{ name: "total", friendly_name: "Total", type: "number" }, { name: "target", friendly_name: "Target", type: "number" }], rows: [{ total: 50, target: 40 }] }}
        options={{ ...counterDefaultOptions, columnName: "total", targetColName: "target" }}
      />,
    );
    const trend = screen.getByText(/vs 40/);
    expect(trend).toHaveClass("text-emerald-600");
  });

  it("shows a down-trend (rose) when value is below the target", () => {
    render(
      <CounterRenderer
        data={{ columns: [{ name: "total", friendly_name: "Total", type: "number" }, { name: "target", friendly_name: "Target", type: "number" }], rows: [{ total: 20, target: 40 }] }}
        options={{ ...counterDefaultOptions, columnName: "total", targetColName: "target" }}
      />,
    );
    const trend = screen.getByText(/vs 40/);
    expect(trend).toHaveClass("text-rose-600");
  });

  describe("number formatting", () => {
    const one = (v: unknown, o: Record<string, unknown> = {}) =>
      render(
        <CounterRenderer
          data={{ columns: data.columns, rows: [{ total: v }] }}
          options={{ ...counterDefaultOptions, columnName: "total", ...o }}
        />,
      );

    it("groups by default", () => {
      one(4222.9);
      expect(screen.getByText("4,222.9")).toBeInTheDocument();
    });
    it("pads to fixed decimals", () => {
      one(4222.9, { numberDecimals: 2 });
      expect(screen.getByText("4,222.90")).toBeInTheDocument();
    });
    it("grouping off + decimals", () => {
      one(4222.9, { numberDecimals: 2, numberGrouping: false });
      expect(screen.getByText("4222.90")).toBeInTheDocument();
    });
    it("keeps huge NUMERIC strings exact", () => {
      one("12345678901234567890.5", { numberDecimals: 0 });
      expect(screen.getByText("12,345,678,901,234,567,891")).toBeInTheDocument();
    });
    it("negatives", () => {
      one(-1234.5);
      expect(screen.getByText("-1,234.5")).toBeInTheDocument();
    });
    it("null renders an em-dash without prefix/suffix", () => {
      one(null, { stringPrefix: "$", stringSuffix: "%" });
      expect(screen.getByText("—")).toBeInTheDocument();
    });
    it("non-numeric and NaN render as-is, keeping prefix", () => {
      one("abc", { stringPrefix: "$" });
      expect(screen.getByTestId("counter-value")).toHaveTextContent(/^\$abc$/);
    });
    it("NaN", () => {
      one(NaN);
      expect(screen.getByText("NaN")).toBeInTheDocument();
    });
    it("options without formatting keys still render grouped", () => {
      render(
        <CounterRenderer
          data={{ columns: data.columns, rows: [{ total: 1234567 }] }}
          options={{ columnName: "total" }}
        />,
      );
      expect(screen.getByText("1,234,567")).toBeInTheDocument();
    });
    it("formats the target and countRow", () => {
      render(
        <CounterRenderer
          data={{
            columns: [...data.columns, { name: "t", friendly_name: "T", type: "number" }],
            rows: [{ total: 6000, t: 5000 }],
          }}
          options={{ ...counterDefaultOptions, columnName: "total", targetColName: "t", numberDecimals: 2 }}
        />,
      );
      expect(screen.getByText("6,000.00")).toBeInTheDocument();
      expect(screen.getByText(/vs 5,000\.00/)).toBeInTheDocument();
    });
  });
});
