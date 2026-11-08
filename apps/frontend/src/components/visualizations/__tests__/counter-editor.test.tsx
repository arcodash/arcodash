import { useState } from "react";
import { describe, it, expect, vi } from "vitest";
import { render, screen, within, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CounterEditor, counterDefaultOptions } from "../counter";
import type { QueryResultData } from "../registry";

/** CounterEditor is fully controlled — wrap it so a click's onChange actually feeds back in,
 * the way VizPanel's real editOptions state does. */
function StatefulCounterEditor({ data, initial }: { data: QueryResultData; initial: Record<string, unknown> }) {
  const [options, setOptions] = useState(initial);
  return <CounterEditor data={data} options={options} onChange={setOptions} />;
}

const data: QueryResultData = {
  columns: [
    { name: "total", friendly_name: "Total", type: "number" },
    { name: "target", friendly_name: "Target", type: "number" },
  ],
  rows: [],
};

describe("CounterEditor", () => {
  it("opening the Target column dropdown never crashes and lists a real-valued 'None' item", async () => {
    const user = userEvent.setup();
    render(<CounterEditor data={data} options={counterDefaultOptions} onChange={() => {}} />);

    const combos = screen.getAllByRole("combobox");
    await user.click(combos[2]); // Target column
    const listbox = await screen.findByRole("listbox");
    expect(within(listbox).getAllByRole("option").map((o) => o.textContent)).toEqual([
      "None",
      "total",
      "target",
    ]);
  });

  it("selecting 'None' on the target column writes targetColName: undefined (not \"\")", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<CounterEditor data={data} options={{ ...counterDefaultOptions, targetColName: "target" }} onChange={onChange} />);

    const combos = screen.getAllByRole("combobox");
    await user.click(combos[2]);
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("None"));
    expect(onChange).toHaveBeenCalledWith({
      ...counterDefaultOptions,
      targetColName: undefined,
    });
  });

  it("selecting a real target column writes its name as a string", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<CounterEditor data={data} options={counterDefaultOptions} onChange={onChange} />);

    const combos = screen.getAllByRole("combobox");
    await user.click(combos[2]);
    const listbox = await screen.findByRole("listbox");
    await user.click(within(listbox).getByText("target"));
    expect(onChange).toHaveBeenCalledWith({ ...counterDefaultOptions, targetColName: "target" });
  });

  it("row number input writes a number, not a string", () => {
    // The Row number <Label> isn't linked via htmlFor/id to its <Input> — locate by role.
    const onChange = vi.fn();
    render(<CounterEditor data={data} options={counterDefaultOptions} onChange={onChange} />);

    fireEvent.change(screen.getByRole("spinbutton"), { target: { value: "3" } });
    expect(onChange).toHaveBeenCalledWith({ ...counterDefaultOptions, rowNumber: 3 });
    expect(typeof onChange.mock.calls[0][0].rowNumber).toBe("number");
  });

  it("prefix/suffix inputs write their exact string values", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<CounterEditor data={data} options={counterDefaultOptions} onChange={onChange} />);

    const [prefixInput, suffixInput] = screen.getAllByRole("textbox");
    await user.type(prefixInput, "$");
    expect(onChange).toHaveBeenLastCalledWith({ ...counterDefaultOptions, stringPrefix: "$" });

    await user.type(suffixInput, "%");
    expect(onChange).toHaveBeenLastCalledWith({ ...counterDefaultOptions, stringSuffix: "%" });
  });

  it("checking 'Count rows' hides the value-column and row-number controls", async () => {
    const user = userEvent.setup();
    render(<StatefulCounterEditor data={data} initial={counterDefaultOptions} />);
    expect(screen.getByRole("spinbutton")).toBeInTheDocument();

    await user.click(screen.getByLabelText("Count rows (instead of column value)"));
    expect(screen.queryByRole("spinbutton")).not.toBeInTheDocument();
  });

  it("Decimal places: picking 2 writes numberDecimals 2; Auto writes undefined", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { unmount } = render(<CounterEditor data={data} options={counterDefaultOptions} onChange={onChange} />);
    await user.click(screen.getAllByRole("combobox")[1]);
    await user.click(within(await screen.findByRole("listbox")).getByRole("option", { name: "2" }));
    expect(onChange).toHaveBeenLastCalledWith({ ...counterDefaultOptions, numberDecimals: 2 });
    unmount();

    render(<CounterEditor data={data} options={{ ...counterDefaultOptions, numberDecimals: 2 }} onChange={onChange} />);
    await user.click(screen.getAllByRole("combobox")[1]);
    await user.click(within(await screen.findByRole("listbox")).getByRole("option", { name: "Auto" }));
    expect(onChange).toHaveBeenLastCalledWith({ ...counterDefaultOptions, numberDecimals: undefined });
  });

  it("unchecking 'Thousands separator' writes numberGrouping false", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<CounterEditor data={data} options={counterDefaultOptions} onChange={onChange} />);
    await user.click(screen.getByLabelText("Thousands separator"));
    expect(onChange).toHaveBeenLastCalledWith({ ...counterDefaultOptions, numberGrouping: false });
  });
});
