import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { TableRenderer, tableDefaultOptions } from "../table";
import type { QueryResultData } from "../registry";

const data: QueryResultData = {
  columns: [
    { name: "name", friendly_name: "Name", type: "string" },
    { name: "n", friendly_name: "N", type: "number" },
  ],
  rows: [
    { name: "a", n: 1 },
    { name: "b", n: 2 },
  ],
};

function rowTexts(container: HTMLElement) {
  return Array.from(container.querySelectorAll("tbody tr")).map((tr) => tr.textContent);
}

describe("TableRenderer", () => {
  it("renders headers and row values given minimal data", () => {
    render(<TableRenderer data={data} options={tableDefaultOptions} />);
    expect(screen.getByText("Name")).toBeInTheDocument();
    expect(screen.getByText("N")).toBeInTheDocument();
    expect(screen.getByText("a")).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument();
  });

  it("shows a placeholder row when there are no rows", () => {
    render(<TableRenderer data={{ columns: data.columns, rows: [] }} options={tableDefaultOptions} />);
    expect(screen.getByText(/no rows returned/i)).toBeInTheDocument();
  });

  it("hides a column marked visible: false", () => {
    render(<TableRenderer data={data} options={{ ...tableDefaultOptions, columns: [{ name: "n", visible: false }] }} />);
    expect(screen.queryByText("N")).not.toBeInTheDocument();
    expect(screen.getByText("Name")).toBeInTheDocument();
  });

  it("uses a column override's friendly_name for the header", () => {
    render(<TableRenderer data={data} options={{ ...tableDefaultOptions, columns: [{ name: "name", friendly_name: "Full Name" }] }} />);
    expect(screen.getByText("Full Name")).toBeInTheDocument();
    expect(screen.queryByText("Name")).not.toBeInTheDocument();
  });

  it("renders null/undefined cells as an em-dash", () => {
    render(<TableRenderer data={{ columns: data.columns, rows: [{ name: null, n: undefined }] }} options={tableDefaultOptions} />);
    expect(screen.getAllByText("—")).toHaveLength(2);
  });

  it("renders object cells as JSON", () => {
    const objData: QueryResultData = {
      columns: [{ name: "payload", friendly_name: "Payload", type: "string" }],
      rows: [{ payload: { a: 1, b: "x" } }],
    };
    render(<TableRenderer data={objData} options={tableDefaultOptions} />);
    expect(screen.getByText('{"a":1,"b":"x"}')).toBeInTheDocument();
  });

  it("sorts rows when a column header is clicked", async () => {
    const user = userEvent.setup();
    const { container } = render(<TableRenderer data={data} options={tableDefaultOptions} />);
    expect(rowTexts(container)).toEqual(["a1", "b2"]);

    await user.click(screen.getByRole("button", { name: /name/i }));
    expect(rowTexts(container)).toEqual(["a1", "b2"]); // ascending, unchanged for already-sorted a,b

    await user.click(screen.getByRole("button", { name: /name/i }));
    expect(rowTexts(container)).toEqual(["b2", "a1"]); // descending
  });

  it("paginates using the configured itemsPerPage", () => {
    const manyRows = Array.from({ length: 5 }, (_, i) => ({ name: `row${i}`, n: i }));
    const { container } = render(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 2 }} />,
    );
    expect(container.querySelectorAll("tbody tr").length).toBe(2);
    expect(screen.getByText(/page 1 of 3/i)).toBeInTheDocument();
  });

  it("itemsPerPage takes effect immediately on rerender, without remounting", () => {
    const manyRows = Array.from({ length: 5 }, (_, i) => ({ name: `row${i}`, n: i }));
    const { container, rerender } = render(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 2 }} />,
    );
    expect(container.querySelectorAll("tbody tr").length).toBe(2);

    rerender(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 5 }} />,
    );
    expect(container.querySelectorAll("tbody tr").length).toBe(5);
    expect(screen.queryByText(/page \d+ of/i)).not.toBeInTheDocument();
  });

  it("changing itemsPerPage resets a stranded pageIndex back to page 1", async () => {
    const user = userEvent.setup();
    const manyRows = Array.from({ length: 5 }, (_, i) => ({ name: `row${i}`, n: i }));
    const { container, rerender } = render(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 1 }} />,
    );
    // Navigate to page 2 of 5 (pageSize 1).
    await user.click(screen.getByRole("button", { name: /next/i }));
    expect(screen.getByText(/page 2 of 5/i)).toBeInTheDocument();

    // Shrinking itemsPerPage past the current pageIndex must not strand the user on an
    // empty page — TanStack doesn't auto-clamp pageIndex when pageSize changes.
    rerender(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 5 }} />,
    );
    expect(container.querySelectorAll("tbody tr").length).toBe(5);
    expect(screen.queryByText(/page \d+ of/i)).not.toBeInTheDocument();

    // Growing back to a smaller-than-total pageSize should also land on page 1, not page 2.
    rerender(
      <TableRenderer data={{ columns: data.columns, rows: manyRows }} options={{ ...tableDefaultOptions, itemsPerPage: 2 }} />,
    );
    expect(screen.getByText(/page 1 of 3/i)).toBeInTheDocument();
    expect(container.querySelectorAll("tbody tr").length).toBe(2);
  });
});
