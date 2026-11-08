import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { ReadonlyField } from "@/components/common/ReadonlyField";

describe("ReadonlyField", () => {
  it("renders the value as full-opacity text, not as a disabled input", () => {
    render(<ReadonlyField label="Email" value="user@example.com" id="email" />);

    const value = screen.getByText("user@example.com");
    expect(value.tagName).toBe("P");
    expect(value).not.toBeDisabled();
    // The canonical pattern: full-opacity foreground text on a muted background, never
    // `disabled:opacity-50` (which makes a real value read as a placeholder).
    expect(value).toHaveClass("text-foreground", "bg-muted/40");
    expect(value).not.toHaveClass("opacity-50");
  });

  it("associates the label with the value via aria-labelledby, not a `for` pointing at a non-labelable <p>", () => {
    render(<ReadonlyField label="Email" value="user@example.com" id="email" />);

    const value = screen.getByText("user@example.com");
    const label = screen.getByText("Email");
    expect(label).not.toHaveAttribute("for");
    expect(value).toHaveAttribute("aria-labelledby", label.id);
    expect(label.id).toBeTruthy();
  });

  it("renders the optional hint, and omits it entirely when absent", () => {
    const { rerender } = render(<ReadonlyField label="Slug" value="default" hint="Fixed once created." />);
    expect(screen.getByText("Fixed once created.")).toBeInTheDocument();

    rerender(<ReadonlyField label="Slug" value="default" />);
    expect(screen.queryByText("Fixed once created.")).not.toBeInTheDocument();
  });

  it("keeps mixed text and element values inline so spaces survive, and lets long values wrap", () => {
    render(
      <ReadonlyField
        label="Measures"
        value={
          <>
            in <span>orders.id</span>
          </>
        }
        id="m"
      />,
    );

    const value = document.getElementById("m")!;
    expect(value.textContent).toBe("in orders.id");
    expect(value).not.toHaveClass("flex");
    expect(value).not.toHaveClass("h-10");
    expect(value).toHaveClass("min-h-10");
  });
});
