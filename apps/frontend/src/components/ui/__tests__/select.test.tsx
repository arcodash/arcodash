import { describe, it, expect } from "vitest";
import { render, screen, fireEvent, act } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";

describe("Select Component", () => {
  it("renders select item with cursor-pointer class", () => {
    render(
      <Select open={true}>
        <SelectTrigger>
          <SelectValue placeholder="Select an item" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="item-1">Item 1</SelectItem>
        </SelectContent>
      </Select>
    );

    const item = screen.getByRole("option", { name: "Item 1" });
    expect(item).toBeInTheDocument();
    expect(item).toHaveClass("cursor-pointer");
    expect(item).not.toHaveClass("cursor-default");
  });

  it("defaults SelectContent to popper positioning (not item-aligned)", () => {
    render(
      <Select open={true}>
        <SelectTrigger>
          <SelectValue placeholder="Select an item" />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="item-1">Item 1</SelectItem>
        </SelectContent>
      </Select>
    );

    // Radix only sets data-side/data-align with position="popper".
    const content = screen.getByRole("listbox");
    expect(content).toHaveAttribute("data-side");
    expect(content).toHaveAttribute("data-align");
  });

  it("does not steal focus back to the trigger from a control the user has since focused", async () => {
    render(
      <div>
        <Select defaultValue="a">
          <SelectTrigger>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="a">A</SelectItem>
            <SelectItem value="b">B</SelectItem>
          </SelectContent>
        </Select>
        <input placeholder="adjacent input" />
      </div>
    );

    // fireEvent keeps these on one tick, inside the window where the popup has closed but its
    // deferred refocus hasn't fired.
    const trigger = screen.getByRole("combobox");
    fireEvent.pointerDown(trigger, { button: 0 });
    fireEvent.click(trigger);
    const optionB = await screen.findByRole("option", { name: "B" });
    fireEvent.pointerUp(optionB);
    fireEvent.click(optionB);

    const input = screen.getByPlaceholderText("adjacent input");
    input.focus();
    expect(document.activeElement).toBe(input);

    // Close-refocus runs in setTimeout(0); flush the event loop (a plain waitFor resolves too
    // early).
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 50));
    });
    expect(document.activeElement).toBe(input);
  });

  it("carries no exit animation on the closing state, only pointer-events-none", () => {
    // Guards a real-browser race: a closing popup holding body pointer-events:none through an
    // animate-out. jsdom can't reproduce it, so assert the closing state has no
    // animate-out/fade-out/zoom-out classes.
    render(
      <Select open={true} defaultValue="a">
        <SelectTrigger>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="a">A</SelectItem>
        </SelectContent>
      </Select>
    );
    const content = document.querySelector('[data-slot="select-content"]');
    expect(content).toHaveClass("data-[state=closed]:pointer-events-none");
    expect(content?.className).not.toMatch(/data-\[state=closed\]:animate-out/);
    expect(content?.className).not.toMatch(/data-\[state=closed\]:fade-out/);
    expect(content?.className).not.toMatch(/data-\[state=closed\]:zoom-out/);
  });

  it("still returns focus to the trigger when nothing else claims it", async () => {
    const user = userEvent.setup();
    render(
      <Select defaultValue="a">
        <SelectTrigger>
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          <SelectItem value="a">A</SelectItem>
          <SelectItem value="b">B</SelectItem>
        </SelectContent>
      </Select>
    );

    const trigger = screen.getByRole("combobox");
    await user.click(trigger);
    await user.click(await screen.findByRole("option", { name: "B" }));

    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 50));
    });
    expect(document.activeElement).toBe(trigger);
  });
});
