import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import type * as ReactRouterModule from "react-router";
import { describe, expect, it, vi } from "vitest";
import { BackButton } from "@/components/nav/BackButton";

const navigateMock = vi.fn();
vi.mock("react-router", async () => {
  const actual = await vi.importActual<typeof ReactRouterModule>("react-router");
  return { ...actual, useNavigate: () => navigateMock };
});

describe("BackButton", () => {
  it("renders default label and navigates to `to` on click", async () => {
    render(<BackButton to="/queries" />, { wrapper: MemoryRouter });
    const button = screen.getByRole("button", { name: "Back" });
    await userEvent.click(button);
    expect(navigateMock).toHaveBeenCalledWith("/queries");
  });

  it("renders a custom label", () => {
    render(<BackButton to="/data_sources" label="Back to data sources" />, { wrapper: MemoryRouter });
    expect(screen.getByRole("button", { name: "Back to data sources" })).toBeInTheDocument();
  });
});
