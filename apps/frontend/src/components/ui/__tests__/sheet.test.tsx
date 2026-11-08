import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import {
  Sheet,
  SheetTrigger,
  SheetContent,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";

function renderSheet(side?: "left" | "right") {
  render(
    <Sheet>
      <SheetTrigger>Open</SheetTrigger>
      <SheetContent side={side}>
        <SheetHeader>
          <SheetTitle>My Sheet</SheetTitle>
        </SheetHeader>
      </SheetContent>
    </Sheet>,
  );
  return userEvent.setup();
}

describe("Sheet", () => {
  it("opens on trigger click and renders content with role=dialog", async () => {
    const user = renderSheet();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.click(screen.getByText("Open"));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("renders the SheetTitle", async () => {
    const user = renderSheet();
    await user.click(screen.getByText("Open"));
    expect(screen.getByText("My Sheet")).toBeInTheDocument();
  });

  it("closes on X click", async () => {
    const user = renderSheet();
    await user.click(screen.getByText("Open"));
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /close/i }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it('applies right-0 class when side="right"', async () => {
    const user = renderSheet("right");
    await user.click(screen.getByText("Open"));
    expect(screen.getByRole("dialog")).toHaveClass("right-0");
  });
});
