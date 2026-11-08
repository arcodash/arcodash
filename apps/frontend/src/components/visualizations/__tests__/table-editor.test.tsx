import { describe, it, expect, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TableEditor, tableDefaultOptions } from "../table";
import type { QueryResultData } from "../registry";

const data: QueryResultData = {
  columns: [
    { name: "name", friendly_name: "Name", type: "string" },
    { name: "n", friendly_name: "N", type: "number" },
  ],
  rows: [],
};

describe("TableEditor", () => {
  it("itemsPerPage input writes a number, not a string", () => {
    const onChange = vi.fn();
    render(<TableEditor data={data} options={tableDefaultOptions} onChange={onChange} />);

    fireEvent.change(screen.getByRole("spinbutton"), { target: { value: "25" } });
    expect(onChange).toHaveBeenCalledWith({ ...tableDefaultOptions, itemsPerPage: 25 });
    expect(typeof onChange.mock.calls[0][0].itemsPerPage).toBe("number");
  });

  it("unchecking a column's visibility writes an override with visible: false", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<TableEditor data={data} options={tableDefaultOptions} onChange={onChange} />);

    const checkboxes = screen.getAllByRole("checkbox");
    await user.click(checkboxes[1]); // second column ("n")
    expect(onChange).toHaveBeenCalledWith({
      ...tableDefaultOptions,
      columns: [{ name: "n", visible: false }],
    });
  });

  it("typing a friendly name writes an override with that exact string", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    render(<TableEditor data={data} options={tableDefaultOptions} onChange={onChange} />);

    const nameOverrideInput = screen.getAllByRole("textbox")[0];
    await user.type(nameOverrideInput, "X");
    expect(onChange).toHaveBeenCalledWith({
      ...tableDefaultOptions,
      columns: [{ name: "name", visible: true, friendly_name: "X" }],
    });
  });

  it("clearing a friendly name override writes friendly_name: undefined", () => {
    const onChange = vi.fn();
    render(
      <TableEditor
        data={data}
        options={{ ...tableDefaultOptions, columns: [{ name: "name", visible: true, friendly_name: "Custom" }] }}
        onChange={onChange}
      />,
    );

    fireEvent.change(screen.getAllByRole("textbox")[0], { target: { value: "" } });
    expect(onChange).toHaveBeenCalledWith({
      ...tableDefaultOptions,
      columns: [{ name: "name", visible: true, friendly_name: undefined }],
    });
  });
});
