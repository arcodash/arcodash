import { describe, it, expect, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ChartEditor, chartDefaultOptions } from "../chart";
import type { QueryResultData } from "../registry";

const data: QueryResultData = {
  columns: [
    { name: "day", friendly_name: "Day", type: "string" },
    { name: "count", friendly_name: "Count", type: "integer" },
    { name: "signups", friendly_name: "Signups", type: "float" },
  ],
  rows: [],
};

// Trigger order in the DOM: chart type, x-axis column, group by, combine rows, stacking.
function combos() {
  return screen.getAllByRole("combobox");
}

describe("ChartEditor", () => {
  it("opening the Stacking dropdown and selecting each option never crashes, and writes the intended options shape", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ChartEditor data={data} options={chartDefaultOptions} onChange={onChange} />);

    await user.click(combos()[4]);
    const listbox = await screen.findByRole("listbox");
    // The sentinel item renders under a real, non-empty value — Radix would reject value="".
    const options = within(listbox).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["None", "Stacked", "100%"]);

    await user.click(within(listbox).getByText("Stacked"));
    expect(onChange).toHaveBeenCalledWith({ ...chartDefaultOptions, stacking: "normal" });

    await user.click(combos()[4]);
    const listbox2 = await screen.findByRole("listbox");
    await user.click(within(listbox2).getByText("100%"));
    expect(onChange).toHaveBeenLastCalledWith({ ...chartDefaultOptions, stacking: "percent" });
  });

  it("selecting 'None' after a real stacking mode writes stacking back to \"\" (not the sentinel)", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ChartEditor data={data} options={{ ...chartDefaultOptions, stacking: "normal" }} onChange={onChange} />);

    await user.click(combos()[4]);
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("None"));
    expect(onChange).toHaveBeenCalledWith({ ...chartDefaultOptions, stacking: "" });
  });

  it("y-axis checkboxes reflect the inferred default series, not a blank form", async () => {
    const user = userEvent.setup();
    // yColumns unset: the renderer falls back to the first non-x column, and the dialog must
    // pre-check it.
    const props = { data, options: { ...chartDefaultOptions, xColumn: "day" } };
    const onChangeAdd = vi.fn();
    const { unmount: unmountAdd } = render(<ChartEditor {...props} onChange={onChangeAdd} />);

    expect(screen.getByLabelText("count")).toBeChecked();
    expect(screen.getByLabelText("signups")).not.toBeChecked();

    await user.click(screen.getByLabelText("signups"));
    expect(onChangeAdd).toHaveBeenLastCalledWith({
      ...chartDefaultOptions,
      xColumn: "day",
      yColumns: ["count", "signups"],
      yColumnsTouched: true,
    });
    unmountAdd();

    const onChangeRemove = vi.fn();
    render(<ChartEditor {...props} onChange={onChangeRemove} />);
    await user.click(screen.getByLabelText("count"));
    expect(onChangeRemove).toHaveBeenLastCalledWith({
      ...chartDefaultOptions,
      xColumn: "day",
      yColumns: [],
      yColumnsTouched: true,
    });
  });

  it("unchecking an explicitly-saved y-axis column writes the exact remaining array", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(
      <ChartEditor data={data} options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"] }} onChange={onChange} />,
    );
    await user.click(screen.getByLabelText("count"));
    expect(onChange).toHaveBeenLastCalledWith({
      ...chartDefaultOptions,
      xColumn: "day",
      yColumns: [],
      yColumnsTouched: true,
    });
  });

  it("unchecking the last y-axis checkbox stays unchecked instead of re-inferring a default", async () => {
    // With yColumnsTouched set, a saved empty array means "no series", not "unset".
    const props = {
      data,
      options: { ...chartDefaultOptions, xColumn: "day", yColumns: [], yColumnsTouched: true },
    };
    render(<ChartEditor {...props} onChange={vi.fn()} />);

    expect(screen.getByLabelText("count")).not.toBeChecked();
    expect(screen.getByLabelText("signups")).not.toBeChecked();
  });

  it("the y-axis list only offers numeric columns", async () => {
    const user = userEvent.setup();
    const withNonNumeric: QueryResultData = {
      columns: [
        ...data.columns,
        { name: "status", friendly_name: "Status", type: "string" },
        { name: "created_at", friendly_name: "Created At", type: "datetime" },
      ],
      rows: [],
    };
    render(<ChartEditor data={withNonNumeric} options={{ ...chartDefaultOptions, xColumn: "day" }} onChange={vi.fn()} />);

    expect(screen.getByLabelText("count")).toBeInTheDocument();
    expect(screen.getByLabelText("signups")).toBeInTheDocument();
    expect(screen.queryByLabelText("status")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("created_at")).not.toBeInTheDocument();

    await user.click(combos()[1]);
    const listbox = await screen.findByRole("listbox");
    expect(within(listbox).getByText("status")).toBeInTheDocument();
  });

  it("switching the x-axis column prunes it out of yColumns so a column never plots against itself", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(
      <ChartEditor
        data={data}
        options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count", "signups"] }}
        onChange={onChange}
      />,
    );

    await user.click(combos()[1]); // x-axis column select
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("count"));

    expect(onChange).toHaveBeenCalledWith({
      ...chartDefaultOptions,
      xColumn: "count",
      yColumns: ["signups"],
    });
  });

  it("toggling legend off writes legend: false", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ChartEditor data={data} options={chartDefaultOptions} onChange={onChange} />);
    await user.click(screen.getByLabelText("Show legend"));
    expect(onChange).toHaveBeenCalledWith({ ...chartDefaultOptions, legend: false });
  });

  it("changing the chart type writes globalSeriesType", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    // Start from an explicit type so the target ("Bar") differs from the inferred default.
    render(
      <ChartEditor data={data} options={{ ...chartDefaultOptions, globalSeriesType: "line" }} onChange={onChange} />,
    );

    await user.click(combos()[0]);
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("Bar"));
    expect(onChange).toHaveBeenCalledWith({ ...chartDefaultOptions, globalSeriesType: "bar" });
  });

  it("group by lists every column except X and Y, non-numeric first, and picking one forces Combine rows to Sum", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const withStore: QueryResultData = {
      columns: [...data.columns, { name: "store", friendly_name: "Store", type: "string" }],
      rows: [],
    };
    render(
      <ChartEditor
        data={withStore}
        options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count"], yColumnsTouched: true }}
        onChange={onChange}
      />,
    );

    await user.click(combos()[2]); // Group by
    const listbox = await screen.findByRole("listbox");
    const options = within(listbox).getAllByRole("option");
    // "day" (X) and "count" (Y) are excluded; "signups" (numeric, unused) sorts after "store".
    expect(options.map((o) => o.textContent)).toEqual(["None", "store", "signups"]);

    await user.click(within(listbox).getByText("store"));
    expect(onChange).toHaveBeenCalledWith({
      ...chartDefaultOptions,
      xColumn: "day",
      yColumns: ["count"],
      yColumnsTouched: true,
      seriesColumn: "store",
      aggregation: "sum",
    });
  });

  it("Combine rows drops 'Don't combine' once a group by is set", async () => {
    const user = userEvent.setup();
    const withStore: QueryResultData = {
      columns: [...data.columns, { name: "store", friendly_name: "Store", type: "string" }],
      rows: [],
    };
    render(
      <ChartEditor
        data={withStore}
        options={{ ...chartDefaultOptions, xColumn: "day", seriesColumn: "store", aggregation: "sum" }}
        onChange={vi.fn()}
      />,
    );

    await user.click(combos()[3]); // Combine rows
    const listbox = await screen.findByRole("listbox");
    const options = within(listbox).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual(["Sum", "Average", "Minimum", "Maximum", "Count of rows"]);
  });

  it("Combine rows offers 'Don't combine' when no group by is set, and writes the chosen aggregation", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<ChartEditor data={data} options={chartDefaultOptions} onChange={onChange} />);

    await user.click(combos()[3]); // Combine rows
    const listbox = await screen.findByRole("listbox");
    const options = within(listbox).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual([
      "Don't combine",
      "Sum",
      "Average",
      "Minimum",
      "Maximum",
      "Count of rows",
    ]);

    await user.click(within(listbox).getByText("Average"));
    expect(onChange).toHaveBeenCalledWith({ ...chartDefaultOptions, aggregation: "avg" });
  });

  it("clearing the group by back to None does not touch the aggregation already chosen", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const withStore: QueryResultData = {
      columns: [...data.columns, { name: "store", friendly_name: "Store", type: "string" }],
      rows: [],
    };
    render(
      <ChartEditor
        data={withStore}
        options={{ ...chartDefaultOptions, xColumn: "day", seriesColumn: "store", aggregation: "avg" }}
        onChange={onChange}
      />,
    );

    await user.click(combos()[2]); // Group by
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("None"));
    expect(onChange).toHaveBeenCalledWith({
      ...chartDefaultOptions,
      xColumn: "day",
      seriesColumn: "",
      aggregation: "avg",
    });
  });

  it("Group by and Combine rows are hidden for pie and scatter, which never aggregate", () => {
    render(
      <ChartEditor
        data={data}
        options={{ ...chartDefaultOptions, globalSeriesType: "pie" }}
        onChange={vi.fn()}
      />,
    );
    expect(screen.queryByText("Group by")).not.toBeInTheDocument();
    expect(screen.queryByText("Combine rows")).not.toBeInTheDocument();
    expect(screen.queryByText("Stacking")).not.toBeInTheDocument();
    expect(combos().length).toBe(2);
  });

  it("Stacking is hidden for line and scatter, and shown for bar and area", () => {
    for (const type of ["line", "scatter"] as const) {
      const { unmount } = render(
        <ChartEditor data={data} options={{ ...chartDefaultOptions, globalSeriesType: type }} onChange={vi.fn()} />,
      );
      expect(screen.queryByText("Stacking")).not.toBeInTheDocument();
      unmount();
    }
    for (const type of ["bar", "area"] as const) {
      const { unmount } = render(
        <ChartEditor data={data} options={{ ...chartDefaultOptions, globalSeriesType: type }} onChange={vi.fn()} />,
      );
      expect(screen.getByText("Stacking")).toBeInTheDocument();
      unmount();
    }
  });

  it("once a group by is set, Y-axis columns become single-select and a prior multi-selection is pruned to one", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const withStore: QueryResultData = {
      columns: [...data.columns, { name: "store", friendly_name: "Store", type: "string" }],
      rows: [],
    };
    render(
      <ChartEditor
        data={withStore}
        options={{ ...chartDefaultOptions, xColumn: "day", yColumns: ["count", "signups"], yColumnsTouched: true }}
        onChange={onChange}
      />,
    );

    await user.click(combos()[2]); // Group by
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("store"));
    // pivotBySeries plots one value column; the second checked box is pruned, not silently ignored.
    expect(onChange).toHaveBeenCalledWith(
      expect.objectContaining({ seriesColumn: "store", yColumns: ["count"] }),
    );
  });

  it("with a group by set, checking a different Y column replaces the selection instead of adding to it", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const withStore: QueryResultData = {
      columns: [...data.columns, { name: "store", friendly_name: "Store", type: "string" }],
      rows: [],
    };
    render(
      <ChartEditor
        data={withStore}
        options={{
          ...chartDefaultOptions,
          xColumn: "day",
          yColumns: ["count"],
          yColumnsTouched: true,
          seriesColumn: "store",
          aggregation: "sum",
        }}
        onChange={onChange}
      />,
    );

    expect(screen.getByLabelText("count")).toHaveAttribute("type", "radio");
    await user.click(screen.getByLabelText("signups"));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({ yColumns: ["signups"] }),
    );
  });
});
