import { describe, it, expect, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { NewResultsPill } from "../NewResultsPill";

describe("NewResultsPill", () => {
  it("renders an empty live region and no button when nothing is new", () => {
    render(<NewResultsPill count={0} onShow={() => {}} />);
    expect(screen.getByRole("status")).toBeEmptyDOMElement();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("announces the count and calls onShow once", async () => {
    const onShow = vi.fn();
    render(<NewResultsPill count={2} onShow={onShow} />);
    expect(screen.getByRole("status")).toHaveTextContent("New results available for 2 tiles");
    await userEvent.click(screen.getByRole("button", { name: "Show new results" }));
    expect(onShow).toHaveBeenCalledTimes(1);
  });
});
