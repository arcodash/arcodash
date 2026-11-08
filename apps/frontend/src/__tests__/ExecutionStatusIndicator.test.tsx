import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ExecutionStatusIndicator } from "@/components/query-editor/ExecutionStatusIndicator";

describe("ExecutionStatusIndicator", () => {
  it("renders Success", () => {
    render(<ExecutionStatusIndicator status="Success" />);
    expect(screen.getByText("Success")).toBeInTheDocument();
  });

  it("renders Failed", () => {
    render(<ExecutionStatusIndicator status="Failed" />);
    expect(screen.getByText("Failed")).toBeInTheDocument();
  });

  it("renders New", () => {
    render(<ExecutionStatusIndicator status="New" />);
    expect(screen.getByText("New")).toBeInTheDocument();
  });
});
