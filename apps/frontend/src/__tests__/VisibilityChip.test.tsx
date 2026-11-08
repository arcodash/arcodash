import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { VisibilityChip } from "@/components/sharing/VisibilityChip";

describe("VisibilityChip", () => {
  it("shows Draft when isDraft is true, regardless of visibility tier", () => {
    render(<VisibilityChip visibility="restricted" isDraft objectType="query" />);
    expect(screen.getByText("Draft")).toBeInTheDocument();
  });

  it("shows Published when isDraft is false — visibility tier never appears on the chip", () => {
    render(<VisibilityChip visibility="workspace" isDraft={false} objectType="dashboard" />);
    expect(screen.getByText("Published")).toBeInTheDocument();
    expect(screen.queryByText("Everyone")).not.toBeInTheDocument();
    expect(screen.queryByText("Specific people")).not.toBeInTheDocument();
  });

  it("falls back to the reach tier when isDraft is omitted (monitors have no publish lifecycle)", () => {
    const { rerender } = render(
      <VisibilityChip visibility="workspace" objectType="monitor" />,
    );
    expect(screen.getByText("Everyone")).toBeInTheDocument();

    rerender(<VisibilityChip visibility="restricted" objectType="monitor" />);
    expect(screen.getByText("Specific people")).toBeInTheDocument();
  });
});
